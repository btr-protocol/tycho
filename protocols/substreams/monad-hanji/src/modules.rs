use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use itertools::Itertools;
use substreams::{
    hex,
    store::{StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetString},
};
use substreams_ethereum::pb::eth::v2::{Block, Log};
use tycho_substreams::{
    block_storage::get_block_storage_changes, entrypoint::create_entrypoint,
    models::entry_point_params::TraceData, prelude::*,
};

/// Topic of the CLOB factory's `OnchainCLOBCreated`; data starts `(market, tokenX, tokenY)`.
const CLOB_CREATED: [u8; 32] =
    hex!("04b3d813686f2d1061e11e5a0d93455065f32174906b0c037cf1176818af86be");
/// Topic of the proxy factory's creation event; data is `(proxy, beacon, market, lpManager)`.
const PROXY_CREATED: [u8; 32] =
    hex!("2ea4d28c98d83eb7f766fbf98625cba73cb302c3a29a0582c6b1711cf6b36b71");
/// Widest price the market accepts (FP24: 999999 * 10^15).
const MAX_PRICE: u128 = 999_999_000_000_000_000_000;

struct Params {
    clob_factory: Vec<u8>,
    proxy_factory: Vec<u8>,
    /// Called by the market on fills only, so no zero-fill trace reaches it.
    watchdog: Vec<u8>,
    /// Quoters the proxy switches to for whitelisted origins only.
    alt_quoters: Vec<Vec<u8>>,
}

impl Params {
    fn parse(params: &str) -> Result<Self> {
        let map: HashMap<&str, &str> = params
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .collect();
        let address = |v: &str| -> Result<Vec<u8>> {
            let a = hex::decode(v.trim_start_matches("0x"))?;
            if a.len() != 20 {
                bail!("not an address: {v}");
            }
            Ok(a)
        };
        let get = |k: &str| {
            map.get(k)
                .ok_or_else(|| anyhow!("missing param {k}"))
        };
        Ok(Self {
            clob_factory: address(get("clob_factory")?)?,
            proxy_factory: address(get("proxy_factory")?)?,
            watchdog: address(get("watchdog")?)?,
            alt_quoters: get("alt_quoters")?
                .split(',')
                .map(address)
                .collect::<Result<_>>()?,
        })
    }
}

/// The `i`-th 32-byte word of a log's data, as an address.
fn address_word(log: &Log, i: usize) -> Option<Vec<u8>> {
    log.data
        .get(32 * i + 12..32 * (i + 1))
        .map(<[u8]>::to_vec)
}

/// `(market, tokenX, tokenY)` of an `OnchainCLOBCreated` log from `factory`.
fn market_created(log: &Log, factory: &[u8]) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    if log.address != factory || log.topics.first()? != &CLOB_CREATED {
        return None;
    }
    Some((address_word(log, 0)?, address_word(log, 1)?, address_word(log, 2)?))
}

/// Tokens of every market the CLOB factory creates, keyed by market address.
#[substreams::handlers::store]
fn store_markets(params: String, block: Block, store: StoreSetString) {
    let params = Params::parse(&params).expect("valid params");
    for log in block.logs() {
        if let Some((market, x, y)) = market_created(log.log, &params.clob_factory) {
            store.set(0, hex::encode(market), &format!("{}:{}", hex::encode(x), hex::encode(y)));
        }
    }
}

#[substreams::handlers::map]
fn map_protocol_changes(
    params: String,
    block: Block,
    markets: StoreGetString,
) -> Result<BlockChanges> {
    let params = Params::parse(&params)?;
    let mut changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();
    // Markets created earlier in this block are not in the store yet.
    let mut new_markets = HashMap::new();

    for log in block.logs() {
        if let Some((market, x, y)) = market_created(log.log, &params.clob_factory) {
            new_markets.insert(market, (x, y));
            continue;
        }
        if log.address() != params.proxy_factory ||
            log.log.topics.first() != Some(&PROXY_CREATED.to_vec())
        {
            continue;
        }
        let (Some(proxy), Some(market), Some(lp_manager)) =
            (address_word(log.log, 0), address_word(log.log, 2), address_word(log.log, 3))
        else {
            bail!(
                "malformed proxy creation log in tx {}",
                hex::encode(&log.receipt.transaction.hash)
            );
        };
        let (x, y) = match new_markets.get(&market) {
            Some(tokens) => tokens.clone(),
            None => {
                // A proxy over a market outside the CLOB factory is not a Hanji market.
                let Some(stored) = markets.get_last(hex::encode(&market)) else {
                    continue;
                };
                let (x, y) = stored
                    .split_once(':')
                    .ok_or_else(|| anyhow!("bad market entry {stored}"))?;
                (hex::decode(x)?, hex::decode(y)?)
            }
        };

        let tx: Transaction = log.receipt.transaction.into();
        let builder = changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        let component = ProtocolComponent::new(&format!("0x{}", hex::encode(&proxy)))
            .with_tokens(&[x.as_slice(), y.as_slice()])
            .as_swap_type("hanji_market", ImplementationType::Vm);
        let wiring = Wiring { proxy, market, lp_manager, tokens: [x, y] };
        for (target, signature, calldata) in wiring.entry_points(&params) {
            let (entrypoint, entrypoint_params) = create_entrypoint(
                target,
                signature.to_string(),
                component.id.clone(),
                TraceData::Rpc(RpcTraceData { caller: None, calldata }),
            );
            builder.add_entrypoint(&entrypoint);
            builder.add_entrypoint_params(&entrypoint_params);
        }
        builder.add_protocol_component(&component);
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: changes
            .into_iter()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        storage_changes: get_block_storage_changes(&block),
    })
}

/// A market's contracts as the proxy factory announces them.
struct Wiring {
    proxy: Vec<u8>,
    market: Vec<u8>,
    lp_manager: Vec<u8>,
    tokens: [Vec<u8>; 2],
}

impl Wiring {
    /// `(target, signature, calldata)` of the entry points the DCI traces for this market:
    /// - a zero-fill market order per side on the proxy: it walks the whole quote path (fast
    ///   quoter, oracle adapter, Pyth, LP manager, market, tries) without moving the caller's
    ///   tokens;
    /// - each token's `balanceOf` the LP manager and the market, and the LP manager's allowance to
    ///   the market: the token slots a fill moves, which the DCI indexes selectively on tokens;
    /// - the watchdog and the alternate fast quoters, which the zero-fill orders do not reach.
    fn entry_points(&self, params: &Params) -> Vec<(Vec<u8>, &'static str, Vec<u8>)> {
        let place_order = "placeOrder(bool,uint128,uint72,uint128,bool,bool,bool,uint256)";
        let mut out = vec![
            (self.proxy.clone(), place_order, zero_fill(true)),
            (self.proxy.clone(), place_order, zero_fill(false)),
        ];
        for token in &self.tokens {
            for owner in [&self.lp_manager, &self.market] {
                out.push((
                    token.clone(),
                    "balanceOf(address)",
                    call(hex!("70a08231"), &[address(owner)]),
                ));
            }
            out.push((
                token.clone(),
                "allowance(address,address)",
                call(hex!("dd62ed3e"), &[address(&self.lp_manager), address(&self.market)]),
            ));
        }
        out.push((params.watchdog.clone(), "isChainStable()", call(hex!("371da77d"), &[])));
        // Unverified quoter view `(uint8 market, bool isAsk, uint128 quantity, uint128 maxValue,
        // uint72 price)`; any market index reaches the quoter's full call path.
        for quoter in &params.alt_quoters {
            out.push((
                quoter.clone(),
                "0x24675a09(uint8,bool,uint128,uint128,uint72)",
                call(hex!("24675a09"), &[uint(0), uint(0), uint(4000), uint(u128::MAX), uint(1)]),
            ));
        }
        out
    }
}

/// `placeOrder` for one share at a price that cannot fill: the proxy has the LP manager quote and
/// the market match, but never pulls the caller's tokens.
fn zero_fill(ask: bool) -> Vec<u8> {
    call(
        hex!("f513ef4c"),
        &[
            uint(ask as u128),
            uint(1),
            uint(if ask { MAX_PRICE } else { 1 }),
            uint(u128::MAX),
            uint(1),
            uint(0),
            uint(1),
            [0xff; 32],
        ],
    )
}

fn call(selector: [u8; 4], words: &[[u8; 32]]) -> Vec<u8> {
    let mut data = selector.to_vec();
    for word in words {
        data.extend_from_slice(word);
    }
    data
}

fn uint(v: u128) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[16..].copy_from_slice(&v.to_be_bytes());
    word
}

fn address(a: &[u8]) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(a);
    word
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_params() {
        let p = Params::parse(
            "clob_factory=0x5C28a12C8EbAF8524A2Ba1fdc62565571Aec87f1&proxy_factory=5c0051d2612045735be095fa0f7c012bbbf87587&watchdog=0xAd31cA82e571cFD5c3E360c658F3200F7c0129c4&alt_quoters=0x95b219bffe843470d0c4350097a2d1e8735a7569,0xe52297e1711d7ae6ab4da9123259b664130ff2cf",
        )
        .unwrap();
        assert_eq!(p.alt_quoters.len(), 2);
        assert_eq!(
            p.proxy_factory,
            hex::decode("5c0051d2612045735be095fa0f7c012bbbf87587").unwrap()
        );
        assert!(Params::parse("clob_factory=0x12").is_err());
    }

    /// Matches `cast calldata "placeOrder(bool,uint128,uint72,uint128,bool,bool,bool,uint256)"
    /// true 1 999999000000000000000 <u128::MAX> true false true <u256::MAX>`.
    #[test]
    fn encodes_zero_fill_order() {
        let data = zero_fill(true);
        assert_eq!(&data[..4], &hex!("f513ef4c"));
        assert_eq!(data.len(), 4 + 8 * 32);
        assert_eq!(data[4 + 31], 1);
        assert_eq!(&data[4 + 2 * 32 + 16..4 + 3 * 32], &MAX_PRICE.to_be_bytes());
        assert_eq!(&data[4 + 7 * 32..], &[0xff; 32]);
    }

    #[test]
    fn reads_creation_logs() {
        let factory = hex!("5c0051d2612045735be095fa0f7c012bbbf87587").to_vec();
        let mut data = Vec::new();
        for a in [[1u8; 20], [2; 20], [3; 20]] {
            data.extend_from_slice(&address(&a));
        }
        let log = Log {
            address: factory.clone(),
            topics: vec![CLOB_CREATED.to_vec()],
            data,
            ..Default::default()
        };
        assert_eq!(market_created(&log, &factory), Some((vec![1; 20], vec![2; 20], vec![3; 20])));
        assert_eq!(market_created(&log, &[0u8; 20]), None);
    }
}
