//! Snapshot source for Kuru markets on Monad, matching the output of the `monad-kuru` package.

use std::collections::HashMap;

use alloy::{
    primitives::{keccak256, Address, B256, U256},
    rpc::types::BlockId,
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use async_trait::async_trait;
use futures03::{stream, StreamExt, TryStreamExt};
use kuru_book::{decode_order, encode_order, slot, slot_attributes, OrderRef};
use num_bigint::BigInt;
use serde::Deserialize;
use tycho_common::{
    models::protocol::{ProtocolComponent, ProtocolComponentState},
    Bytes,
};
use tycho_ethereum::rpc::EthereumRpcClient;
use tycho_protobuf::pb::tycho::evm::v1 as pb;

use crate::extractor::{
    bootstrap::{logs::LogSource, SnapshotSource},
    ExtractionError,
};

sol! {
    event MarketRegistered(
        address baseAsset,
        address quoteAsset,
        address market,
        address vaultAddress,
        uint32 pricePrecision,
        uint96 sizePrecision,
        uint32 tickSize,
        uint96 minSize,
        uint96 maxSize,
        uint256 takerFeeBps,
        uint256 makerFeeBps,
        uint96 kuruAmmSpread
    );

    function getL2Book() external view returns (bytes memory);
    function getMarketParams() external view returns (
        uint32 pricePrecision,
        uint96 sizePrecision,
        address baseAsset,
        uint256 baseDecimals,
        address quoteAsset,
        uint256 quoteDecimals,
        uint32 tickSize,
        uint96 minSize,
        uint96 maxSize,
        uint256 takerFeeBps,
        uint256 makerFeeBps
    );
    function s_orders(uint40 id) external view returns (
        address ownerAddress,
        uint96 size,
        uint40 prev,
        uint40 next,
        uint40 flippedId,
        uint32 price,
        uint32 flippedPrice,
        bool isBuy
    );
}

#[derive(Debug, Deserialize, Clone)]
pub struct KuruConfig {
    /// The Kuru Router whose `MarketRegistered` logs define the components.
    pub router: Address,
}

/// Reads Kuru markets: the storage words the package mirrors as attributes, the price levels of
/// `getL2Book`, every resting order (walking each price point's order list), and the book depth
/// as balances.
pub struct KuruSource {
    config: KuruConfig,
    rpc: EthereumRpcClient,
    logs: Box<dyn LogSource>,
    calls_per_request: usize,
    concurrency: usize,
}

fn setup(what: String) -> impl FnOnce(tycho_ethereum::rpc::errors::RPCError) -> ExtractionError {
    move |e| ExtractionError::Setup(format!("{what}: {e}"))
}

fn decode_error(what: &str, market: Address, e: impl std::fmt::Display) -> ExtractionError {
    ExtractionError::Setup(format!("Failed to decode {what} of market {market}: {e}"))
}

fn signed(value: impl Into<BigInt>) -> Vec<u8> {
    value.into().to_signed_bytes_be()
}

/// A market's parameters, from its component's static attributes.
struct Market {
    address: Address,
    pp: u128,
    sp: u128,
    /// `None` when the market's decimals are unknown: the package then keeps no balances.
    base_mult: Option<BigInt>,
    quote_mult: Option<BigInt>,
    base: Bytes,
    quote: Bytes,
    base_decimals: Option<u32>,
    quote_decimals: Option<u32>,
}

impl Market {
    fn from_component(component: &ProtocolComponent) -> Result<Self, ExtractionError> {
        let get = |name: &str| {
            component
                .static_attributes
                .get(name)
                .map(|v| BigInt::from_signed_bytes_be(v))
        };
        let number = |name: &str| -> Result<u128, ExtractionError> {
            get(name)
                .and_then(|v| u128::try_from(v).ok())
                .ok_or_else(|| {
                    ExtractionError::Setup(format!("Component {} has no {name}", component.id))
                })
        };
        let token = |name: &str| {
            component
                .static_attributes
                .get(name)
                .cloned()
                .ok_or_else(|| {
                    ExtractionError::Setup(format!("Component {} has no {name}", component.id))
                })
        };
        let (base, quote) = (token("base")?, token("quote")?);
        let decimals = |name: &str| get(name).and_then(|v| u32::try_from(v).ok());
        let (base_decimals, quote_decimals) =
            (decimals("base_decimals"), decimals("quote_decimals"));
        // As the package: native MON has 18 decimals, other tokens without decimals none.
        let mult = |d: Option<u32>, token: &Bytes| {
            d.or(token
                .iter()
                .all(|b| *b == 0)
                .then_some(18))
                .map(|d| BigInt::from(10).pow(d))
        };
        Ok(Self {
            address: component.id.parse().map_err(|e| {
                ExtractionError::Setup(format!(
                    "Component id {} is not an address: {e}",
                    component.id
                ))
            })?,
            pp: number("price_precision")?,
            sp: number("size_precision")?,
            base_mult: mult(base_decimals, &base),
            quote_mult: mult(quote_decimals, &quote),
            base,
            quote,
            base_decimals,
            quote_decimals,
        })
    }

    /// Token amount of a level, as the package converts a level delta.
    fn level_amount(&self, is_buy: bool, price: u32, size: u128) -> Option<BigInt> {
        let (size, sp) = (BigInt::from(size), BigInt::from(self.sp));
        if is_buy {
            Some(
                size * BigInt::from(price) * self.quote_mult.clone()? /
                    (sp * BigInt::from(self.pp)),
            )
        } else {
            Some(size * self.base_mult.clone()? / sp)
        }
    }
}

/// A book level: (is_buy, price, size).
type Level = (bool, u32, u128);

/// `getL2Book()` bytes: block number, `(price, size)` bids best first, a zero word, asks best
/// first.
fn parse_l2(data: &[u8]) -> Result<Vec<Level>, String> {
    if !data.len().is_multiple_of(32) || data.len() < 32 {
        return Err(format!("length {}", data.len()));
    }
    let words: Vec<U256> = data
        .chunks(32)
        .map(U256::from_be_slice)
        .collect();
    let mut levels = Vec::new();
    let (mut i, mut is_buy) = (1, true);
    while i < words.len() {
        if is_buy && words[i].is_zero() {
            is_buy = false;
            i += 1;
            continue;
        }
        let price = u32::try_from(words[i]).map_err(|_| "price above uint32")?;
        let size = words.get(i + 1).ok_or("truncated")?;
        levels.push((is_buy, price, u128::try_from(*size).map_err(|_| "size above uint128")?));
        i += 2;
    }
    Ok(levels)
}

/// Storage key of `price`'s price point in the bid or ask mapping.
fn price_point_key(is_buy: bool, price: u32) -> B256 {
    let mapping = if is_buy { slot::BUY_PRICE_POINTS } else { slot::SELL_PRICE_POINTS };
    keccak256((U256::from(price), U256::from(mapping)).abi_encode())
}

impl KuruSource {
    pub fn new(
        config: KuruConfig,
        rpc: EthereumRpcClient,
        logs: Box<dyn LogSource>,
        calls_per_request: usize,
        concurrency: usize,
    ) -> Self {
        Self { config, rpc, logs, calls_per_request, concurrency }
    }

    async fn multicall(
        &self,
        calls: Vec<(Address, alloy::primitives::Bytes)>,
        block: BlockId,
    ) -> Result<Vec<alloy::primitives::Bytes>, ExtractionError> {
        self.rpc
            .multicall(&calls, block, self.calls_per_request, self.concurrency)
            .await
            .map_err(setup(format!("Snapshot read at block {block}")))
    }

    /// Storage words of `(market, key)` at `block`, in order.
    async fn storage(
        &self,
        keys: Vec<(Address, B256)>,
        block: BlockId,
    ) -> Result<Vec<B256>, ExtractionError> {
        stream::iter(keys)
            .map(|(market, key)| async move {
                self.rpc
                    .get_storage_at(market, key, block)
                    .await
                    .map_err(setup(format!("Storage {key} of market {market}")))
            })
            .buffered(self.concurrency)
            .try_collect()
            .await
    }

    /// Components of `markets` as the package emits them, with parameters read at `block`.
    pub async fn read_components(
        &self,
        markets: &[Address],
        block: BlockId,
    ) -> Result<Vec<pb::ProtocolComponent>, ExtractionError> {
        let params = self
            .multicall(
                markets
                    .iter()
                    .map(|m| {
                        (
                            *m,
                            getMarketParamsCall {}
                                .abi_encode()
                                .into(),
                        )
                    })
                    .collect(),
                block,
            )
            .await?;
        markets
            .iter()
            .zip(params)
            .map(|(market, data)| {
                let p = getMarketParamsCall::abi_decode_returns(&data)
                    .map_err(|e| decode_error("getMarketParams", *market, e))?;
                let creation = |name: &str, value: Vec<u8>| pb::Attribute {
                    name: name.to_string(),
                    value,
                    change: pb::ChangeType::Creation.into(),
                };
                let uint = |v: U256| {
                    signed(BigInt::from_bytes_be(num_bigint::Sign::Plus, &v.to_be_bytes::<32>()))
                };
                Ok(pb::ProtocolComponent {
                    id: format!("0x{}", hex::encode(market)),
                    tokens: vec![p.baseAsset.to_vec(), p.quoteAsset.to_vec()],
                    contracts: vec![],
                    static_att: vec![
                        creation("base", p.baseAsset.to_vec()),
                        creation("quote", p.quoteAsset.to_vec()),
                        creation("price_precision", signed(p.pricePrecision)),
                        creation("size_precision", signed(p.sizePrecision.to::<u128>())),
                        creation("base_decimals", uint(p.baseDecimals)),
                        creation("quote_decimals", uint(p.quoteDecimals)),
                    ],
                    change: pb::ChangeType::Creation.into(),
                    protocol_type: Some(pb::ProtocolType {
                        name: "kuru_market".to_string(),
                        financial_type: pb::FinancialType::Swap.into(),
                        attribute_schema: vec![],
                        implementation_type: pb::ImplementationType::Custom.into(),
                    }),
                })
            })
            .collect()
    }

    /// Every resting order of `levels` (per market), walking each price point's list from its
    /// head through `s_orders(id).next`. The orders of a level must sum to its size.
    async fn resting_orders(
        &self,
        levels: &[(Address, Vec<Level>)],
        block: BlockId,
    ) -> Result<Vec<Vec<(u64, OrderRef)>>, ExtractionError> {
        let points: Vec<(usize, bool, u32, u128)> = levels
            .iter()
            .enumerate()
            .flat_map(|(i, (_, lv))| {
                lv.iter()
                    .map(move |(is_buy, price, size)| (i, *is_buy, *price, *size))
            })
            .collect();
        let heads = self
            .storage(
                points
                    .iter()
                    .map(|(i, is_buy, price, _)| (levels[*i].0, price_point_key(*is_buy, *price)))
                    .collect(),
                block,
            )
            .await?;
        // (point, next order id) still to read
        let mut cursor: Vec<(usize, u64)> = heads
            .iter()
            .enumerate()
            .map(|(p, word)| {
                (p, (U256::from_be_bytes(word.0) & U256::from(0xff_ffff_ffffu64)).to::<u64>())
            })
            .filter(|(_, id)| *id != 0)
            .collect();
        let mut orders: Vec<Vec<(u64, OrderRef)>> = vec![Vec::new(); levels.len()];
        let mut sums = vec![0u128; points.len()];
        while !cursor.is_empty() {
            let calls = cursor
                .iter()
                .map(|(p, id)| {
                    let call = s_ordersCall { id: alloy::primitives::Uint::from(*id) };
                    (levels[points[*p].0].0, call.abi_encode().into())
                })
                .collect();
            let results = self.multicall(calls, block).await?;
            let mut next = Vec::new();
            for ((p, id), data) in cursor.into_iter().zip(results) {
                let (i, is_buy, price, _) = points[p];
                let o = s_ordersCall::abi_decode_returns(&data)
                    .map_err(|e| decode_error("s_orders", levels[i].0, e))?;
                if o.price != price || o.isBuy != is_buy {
                    return Err(ExtractionError::Setup(format!(
                        "Order {id} of market {} is not at the price point that lists it",
                        levels[i].0
                    )));
                }
                let size = o.size.to::<u128>();
                sums[p] += size;
                orders[i].push((id, (price, is_buy, size)));
                let following = o.next.to::<u64>();
                if following != 0 {
                    next.push((p, following));
                }
            }
            cursor = next;
        }
        for (p, (i, is_buy, price, size)) in points.iter().enumerate() {
            if sums[p] != *size {
                return Err(ExtractionError::Setup(format!(
                    "Orders of market {} {} {price} sum to {}, not the level's {size}",
                    levels[*i].0,
                    if *is_buy { "bid" } else { "ask" },
                    sums[p]
                )));
            }
        }
        Ok(orders)
    }
}

#[async_trait]
impl SnapshotSource for KuruSource {
    /// Rows `m:<market>:<pp>:<sp>:<base_dec>:<quote_dec>:<base>:<quote>` for every market and
    /// `o:<market>:<id>:<price>:<b|a>:<size>` for every resting order, which seed the package's
    /// market and order stores.
    fn snapshot_rows(
        &self,
        _block: u64,
        components: &[ProtocolComponent],
        states: &[ProtocolComponentState],
    ) -> Result<Vec<String>, ExtractionError> {
        let mut rows = Vec::new();
        for component in components {
            let m = Market::from_component(component)?;
            let dec = |d: Option<u32>| d.map_or_else(String::new, |d| d.to_string());
            rows.push(format!(
                "m:{}:{}:{}:{}:{}:{}:{}",
                component.id,
                m.pp,
                m.sp,
                dec(m.base_decimals),
                dec(m.quote_decimals),
                m.base,
                m.quote
            ));
        }
        for state in states {
            for (name, value) in &state.attributes {
                let Some(id) = name.strip_prefix("o/") else { continue };
                let (price, is_buy, size) = decode_order(value).map_err(|e| {
                    ExtractionError::Setup(format!("{} {name}: {e}", state.component_id))
                })?;
                rows.push(format!(
                    "o:{}:{id}:{price}:{}:{size}",
                    state.component_id,
                    if is_buy { "b" } else { "a" }
                ));
            }
        }
        Ok(rows)
    }

    async fn components(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<pb::ProtocolComponent>, ExtractionError> {
        let logs = self
            .logs
            .logs(self.config.router, MarketRegistered::SIGNATURE_HASH, from, to)
            .await?;
        let markets = logs
            .into_iter()
            .map(|log| {
                MarketRegistered::decode_raw_log(log.topics.iter().copied(), &log.data)
                    .map(|e| e.market)
                    .map_err(|e| {
                        ExtractionError::Setup(format!(
                            "Invalid MarketRegistered log in tx {}: {e}",
                            log.transaction_hash
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.read_components(&markets, to.into())
            .await
    }

    async fn state(
        &self,
        components: &[ProtocolComponent],
        block: BlockId,
    ) -> Result<Vec<ProtocolComponentState>, ExtractionError> {
        let markets = components
            .iter()
            .map(Market::from_component)
            .collect::<Result<Vec<_>, _>>()?;
        let books = self
            .multicall(
                markets
                    .iter()
                    .map(|m| (m.address, getL2BookCall {}.abi_encode().into()))
                    .collect(),
                block,
            )
            .await?;
        let levels = markets
            .iter()
            .zip(books)
            .map(|(m, data)| {
                let raw = getL2BookCall::abi_decode_returns(&data)
                    .map_err(|e| decode_error("getL2Book", m.address, e))?;
                let levels = parse_l2(&raw).map_err(|e| decode_error("getL2Book", m.address, e))?;
                Ok((m.address, levels))
            })
            .collect::<Result<Vec<_>, ExtractionError>>()?;
        let orders = self
            .resting_orders(&levels, block)
            .await?;
        let words = self
            .storage(
                markets
                    .iter()
                    .flat_map(|m| {
                        slot::ATTRIBUTES
                            .iter()
                            .map(|s| (m.address, B256::with_last_byte(*s)))
                    })
                    .collect(),
                block,
            )
            .await?;

        let mut states = Vec::with_capacity(markets.len());
        for (((m, (_, levels)), orders), words) in markets
            .iter()
            .zip(levels)
            .zip(orders)
            .zip(words.chunks(slot::ATTRIBUTES.len()))
        {
            let mut attributes: HashMap<String, Bytes> = HashMap::new();
            for (s, word) in slot::ATTRIBUTES.iter().zip(words) {
                for (name, value) in slot_attributes(*s, &word.0) {
                    attributes.insert(name.to_string(), value.into());
                }
            }
            let mut depth: HashMap<&Bytes, BigInt> = HashMap::new();
            for (is_buy, price, size) in &levels {
                let side = if *is_buy { "b" } else { "a" };
                attributes.insert(format!("{side}/{price}"), signed(*size).into());
                if let Some(amount) = m.level_amount(*is_buy, *price, *size) {
                    *depth
                        .entry(if *is_buy { &m.quote } else { &m.base })
                        .or_default() += amount;
                }
            }
            for (id, order) in orders {
                attributes.insert(format!("o/{id}"), encode_order(order).to_vec().into());
            }
            let balances = if m.base_mult.is_some() && m.quote_mult.is_some() {
                [&m.base, &m.quote]
                    .into_iter()
                    .map(|token| {
                        let amount = depth
                            .get(token)
                            .cloned()
                            .unwrap_or_default();
                        (token.clone(), Bytes::from(amount.to_bytes_be().1))
                    })
                    .collect()
            } else {
                HashMap::new()
            };
            states.push(ProtocolComponentState::new(
                &format!("0x{}", hex::encode(m.address)),
                attributes,
                balances,
            ));
        }
        Ok(states)
    }
}

#[cfg(test)]
mod tests {
    use alloy::rpc::types::{Filter, Log};

    use super::*;
    use crate::extractor::bootstrap::logs::RpcLogSource;

    #[test]
    fn test_parse_l2() {
        let word = |v: u64| {
            U256::from(v)
                .to_be_bytes::<32>()
                .to_vec()
        };
        let data = [word(9), word(100), word(5), word(0), word(101), word(7)].concat();
        assert_eq!(parse_l2(&data).unwrap(), vec![(true, 100, 5), (false, 101, 7)]);
        assert!(parse_l2(&data[..70]).is_err());
    }

    #[test]
    fn test_price_point_key_matches_chain() {
        // `cast index uint256 2681200 51`: the bid price point of MON/USDC read 09-30
        assert_eq!(
            price_point_key(true, 2_681_200),
            keccak256((U256::from(2_681_200u32), U256::from(51u8)).abi_encode())
        );
        assert_ne!(price_point_key(true, 1), price_point_key(false, 1));
    }

    fn component(id: &str) -> ProtocolComponent {
        let be = |v: u128| Bytes::from(signed(v));
        ProtocolComponent {
            id: id.to_string(),
            static_attributes: HashMap::from([
                ("base".to_string(), Bytes::from(vec![0u8; 20])),
                ("quote".to_string(), Bytes::from(vec![1u8; 20])),
                ("price_precision".to_string(), be(100)),
                ("size_precision".to_string(), be(1000)),
                ("quote_decimals".to_string(), be(6)),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn test_snapshot_rows_list_markets_and_orders() {
        let rpc = EthereumRpcClient::new("http://127.0.0.1:1").unwrap();
        let source = KuruSource::new(
            KuruConfig { router: Address::ZERO },
            rpc.clone(),
            Box::new(RpcLogSource::new(rpc, 1, 1)),
            1,
            1,
        );
        let id = "0x00000000000000000000000000000000000000aa";
        let state = ProtocolComponentState::new(
            id,
            HashMap::from([
                ("o/7".to_string(), Bytes::from(encode_order((12, true, 5)).to_vec())),
                ("b/12".to_string(), Bytes::from(vec![5])),
            ]),
            HashMap::new(),
        );
        let mut rows = source
            .snapshot_rows(9, &[component(id)], &[state])
            .unwrap();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                format!("m:{id}:100:1000::6:0x{}:0x{}", "00".repeat(20), "01".repeat(20)),
                format!("o:{id}:7:12:b:5"),
            ]
        );
    }

    #[test]
    fn test_level_amount_matches_package_conversion() {
        let m = Market::from_component(&component("0x00000000000000000000000000000000000000aa"))
            .unwrap();
        // bids in quote: size * price * 10^6 / (sp * pp); asks in base (native, 18 decimals)
        assert_eq!(m.level_amount(true, 50, 3), Some(BigInt::from(3 * 50 * 1_000_000 / 100_000)));
        assert_eq!(m.level_amount(false, 50, 3), Some(BigInt::from(3u128 * 10u128.pow(18) / 1000)));
    }

    fn env_or(name: &str, default: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| default.to_string())
    }

    fn hex_state(state: &ProtocolComponentState) -> serde_json::Value {
        serde_json::json!({
            "attributes": state
                .attributes
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::from(v.to_string())))
                .collect::<serde_json::Map<_, _>>(),
            "balances": state
                .balances
                .iter()
                .map(|(k, v)| (k.to_string(), serde_json::Value::from(v.to_string())))
                .collect::<serde_json::Map<_, _>>(),
        })
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

    /// Records a bootstrap case into the replay fixture of the `monad-kuru` package, whose
    /// `test_replay_matches_chain` replays it: the snapshot of `MARKETS` at a finalized block N,
    /// the parameters the indexer passes, every market log over N+1..=N+SPAN, and the snapshot at
    /// N+SPAN.
    #[tokio::test]
    #[ignore = "Requires RPC_URL with historical eth_call; LOGS_RPC_URL, MARKETS, SPAN and \
                LOGS_BLOCK_RANGE are optional"]
    async fn test_generate_kuru_replay_fixture() {
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
        let markets: Vec<Address> = env_or(
            "MARKETS",
            "0x065c9d28e428a0db40191a54d33d5b7c71a9c394,0x122c0d8683cab344163fb73e28e741754257e3fa",
        )
        .split(',')
        .map(|m| m.parse().unwrap())
        .collect();
        let span: u64 = env_or("SPAN", "2000").parse().unwrap();
        let range: u64 = env_or("LOGS_BLOCK_RANGE", "100")
            .parse()
            .unwrap();
        let end = rpc.get_block_number().await.unwrap() - 100;
        let start = end - span;
        let source = KuruSource::new(
            KuruConfig { router: Address::ZERO },
            rpc.clone(),
            Box::new(RpcLogSource::new(logs_rpc.clone(), range, 4)),
            200,
            4,
        );

        let pb_components = source
            .read_components(&markets, start.into())
            .await
            .unwrap();
        let components: Vec<ProtocolComponent> = pb_components
            .iter()
            .map(|c| ProtocolComponent {
                id: c.id.clone(),
                tokens: c
                    .tokens
                    .iter()
                    .map(|t| t.clone().into())
                    .collect(),
                static_attributes: c
                    .static_att
                    .iter()
                    .map(|a| (a.name.clone(), a.value.clone().into()))
                    .collect(),
                ..Default::default()
            })
            .collect();
        let start_states = source
            .state(&components, start.into())
            .await
            .unwrap();
        let params = crate::extractor::bootstrap::snapshot_chunks(
            start,
            source
                .snapshot_rows(start, &components, &start_states)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(params.len(), 1, "fixture snapshot spans several chunks");

        let mut logs = Vec::new();
        let mut from = start + 1;
        while from <= end {
            let to = end.min(from + range - 1);
            logs.extend(
                logs_rpc
                    .get_logs(
                        &Filter::new()
                            .address(markets.clone())
                            .from_block(from)
                            .to_block(to),
                    )
                    .await
                    .unwrap(),
            );
            from = to + 1;
        }
        let end_states = source
            .state(&components, end.into())
            .await
            .unwrap();
        let states = |states: &[ProtocolComponentState]| {
            states
                .iter()
                .map(|s| (s.component_id.clone(), hex_state(s)))
                .collect::<serde_json::Map<_, _>>()
        };
        let orders = |states: &[ProtocolComponentState]| {
            states
                .iter()
                .flat_map(|s| s.attributes.keys())
                .filter(|k| k.starts_with("o/"))
                .count()
        };
        println!(
            "N {start} N+k {end} markets {} orders {}->{} logs {} params {} bytes",
            markets.len(),
            orders(&start_states),
            orders(&end_states),
            logs.len(),
            params[0].len()
        );
        let case = serde_json::json!({
            "name": "bootstrap",
            "start_block": start,
            "end_block": end,
            "snapshot_params": params[0],
            "start": states(&start_states),
            "logs": logs.iter().map(log_json).collect::<Vec<_>>(),
            "end": states(&end_states),
        });
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocols/substreams/monad-kuru/tests/fixtures/replay.json"
        );
        std::fs::create_dir_all(
            std::path::Path::new(path)
                .parent()
                .unwrap(),
        )
        .unwrap();
        std::fs::write(
            path,
            serde_json::to_string_pretty(&serde_json::json!({ "cases": [case] })).unwrap(),
        )
        .unwrap();
    }
}
