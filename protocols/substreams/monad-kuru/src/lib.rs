//! Kuru (Monad CLOB) for Tycho.
//!
//! Components = markets registered by the Kuru Router. Book levels come from market events via the
//! shared `kuru_book` rules; vault quotes, market state and fees come from the market's storage
//! (Monad blocks are Extended), because vault fills move storage without a complete event trail.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod abi;
mod pb;
#[cfg(test)]
mod replay_tests;

use std::{collections::HashMap, str::FromStr};

use abi::{order_book::events as ob, router::events::MarketRegistered};
use anyhow::{anyhow, bail, Context};
use kuru_book::{encode_order, field, slot, slot_attributes, Event as BookEvent};
use pb::{Event, Events, LevelDelta, LevelDeltas, OrderUpdate, Snapshot, SnapshotMarket};
use substreams::{
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{
        StoreDelete, StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetBigInt,
        StoreSetIfNotExists, StoreSetIfNotExistsString, StoreSetString,
    },
};
use substreams_ethereum::{pb::eth::v2 as eth, Event as _};
use tycho_substreams::{
    prelude::*,
    snapshot::{fields, rows},
};

fn hex0x(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

fn slot_of(key: &[u8]) -> Option<u8> {
    (key.len() == 32 && key[..31].iter().all(|b| *b == 0)).then(|| key[31])
}

/// A storage word, left-padded to 32 bytes.
fn word(value: &[u8]) -> [u8; 32] {
    let mut w = [0u8; 32];
    let n = value.len().min(32);
    w[32 - n..].copy_from_slice(&value[value.len() - n..]);
    w
}

fn attr(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Update.into() }
}

/// Static market parameters kept per component: "pp,sp,base_mult_exp,quote_mult_exp,base,quote".
struct Market {
    pp: u128,
    sp: u128,
    base_dec: u32,
    quote_dec: u32,
    base: Vec<u8>,
    quote: Vec<u8>,
}

impl Market {
    /// Decimals the registering transaction did not write are 18 for native MON, else unknown
    /// (`u32::MAX`: no balances).
    fn new(
        pp: u128,
        sp: u128,
        base_dec: Option<u32>,
        quote_dec: Option<u32>,
        base: Vec<u8>,
        quote: Vec<u8>,
    ) -> Market {
        let dec = |d: Option<u32>, token: &[u8]| {
            d.or(token
                .iter()
                .all(|b| *b == 0)
                .then_some(18))
                .unwrap_or(u32::MAX)
        };
        Market {
            pp,
            sp,
            base_dec: dec(base_dec, &base),
            quote_dec: dec(quote_dec, &quote),
            base,
            quote,
        }
    }

    fn encode(&self) -> String {
        format!(
            "{},{},{},{},{},{}",
            self.pp,
            self.sp,
            self.base_dec,
            self.quote_dec,
            hex::encode(&self.base),
            hex::encode(&self.quote)
        )
    }
    fn decode(s: &str) -> Market {
        let p: Vec<&str> = s.split(',').collect();
        Market {
            pp: p[0].parse().unwrap(),
            sp: p[1].parse().unwrap(),
            base_dec: p[2].parse().unwrap(),
            quote_dec: p[3].parse().unwrap(),
            base: hex::decode(p[4]).unwrap(),
            quote: hex::decode(p[5]).unwrap(),
        }
    }
}

/// Decimals the market's `initialize` wrote in the registering transaction.
fn decimals(tx: &eth::TransactionTrace, market: &[u8]) -> (Option<u32>, Option<u32>) {
    let (mut b, mut q) = (None, None);
    for sc in tx
        .calls
        .iter()
        .filter(|c| !c.state_reverted)
        .flat_map(|c| c.storage_changes.iter())
        .filter(|sc| sc.address == market)
    {
        let v = BigInt::from_unsigned_bytes_be(&sc.new_value).to_u64() as u32;
        match slot_of(&sc.key) {
            Some(slot::BASE_DECIMALS) => b = Some(v),
            Some(slot::QUOTE_DECIMALS) => q = Some(v),
            _ => {}
        }
    }
    (b, q)
}

#[substreams::handlers::map]
fn map_markets(
    params: String,
    block: eth::Block,
) -> Result<BlockTransactionProtocolComponents, substreams::errors::Error> {
    let router = hex::decode(params.trim_start_matches("0x"))?;
    let mut out = Vec::new();
    for tx in block.transactions() {
        let components: Vec<ProtocolComponent> = tx
            .logs_with_calls()
            .filter(|(log, _)| log.address == router)
            .filter_map(|(log, _)| MarketRegistered::match_and_decode(log))
            .map(|ev| {
                let (bdec, qdec) = decimals(tx, &ev.market);
                let mut static_att = vec![
                    ("base", ev.base_asset.clone()),
                    ("quote", ev.quote_asset.clone()),
                    ("price_precision", ev.price_precision.to_signed_bytes_be()),
                    ("size_precision", ev.size_precision.to_signed_bytes_be()),
                ];
                if let (Some(b), Some(q)) = (bdec, qdec) {
                    static_att.push(("base_decimals", BigInt::from(b).to_signed_bytes_be()));
                    static_att.push(("quote_decimals", BigInt::from(q).to_signed_bytes_be()));
                }
                ProtocolComponent {
                    id: hex0x(&ev.market),
                    tokens: vec![ev.base_asset.clone(), ev.quote_asset.clone()],
                    contracts: vec![],
                    static_att: static_att
                        .into_iter()
                        .map(|(n, v)| Attribute {
                            name: n.into(),
                            value: v,
                            change: ChangeType::Creation.into(),
                        })
                        .collect(),
                    change: ChangeType::Creation.into(),
                    protocol_type: Some(ProtocolType {
                        name: "kuru_market".into(),
                        financial_type: FinancialType::Swap.into(),
                        attribute_schema: vec![],
                        implementation_type: ImplementationType::Custom.into(),
                    }),
                }
            })
            .collect();
        if !components.is_empty() {
            out.push(TransactionProtocolComponents { tx: Some(tx.into()), components });
        }
    }
    Ok(BlockTransactionProtocolComponents { tx_components: out })
}

// Markets and resting orders of a state snapshot at block N, emitted on N+1 so the market and
// order stores start from them. Rows: `m:<market>:<pp>:<sp>:<base_dec>:<quote_dec>:<base>:<quote>`
// (a decimals field empty when the registering transaction wrote none) and
// `o:<market>:<id>:<price>:<b|a>:<size>`; addresses in 0x-hex, numbers in decimal.
tycho_substreams::snapshot_modules!(Snapshot, snapshot);

fn snapshot(params: &str, block: u64) -> Result<Snapshot, anyhow::Error> {
    let mut out = Snapshot::default();
    for row in rows(params, block)? {
        match row.split_once(':') {
            Some(("m", rest)) => {
                let [market, pp, sp, bdec, qdec, base, quote] = fields(rest)?;
                let dec = |d: &str| {
                    (!d.is_empty())
                        .then(|| d.parse())
                        .transpose()
                };
                let m = Market::new(
                    pp.parse().context("pp")?,
                    sp.parse().context("sp")?,
                    dec(bdec).context("base decimals")?,
                    dec(qdec).context("quote decimals")?,
                    tycho_substreams::snapshot::hex_field(base)?,
                    tycho_substreams::snapshot::hex_field(quote)?,
                );
                out.markets
                    .push(SnapshotMarket { market: market.to_lowercase(), params: m.encode() });
            }
            Some(("o", rest)) => {
                let [market, id, price, side, size] = fields(rest)?;
                u128::from_str(size).context("order size")?;
                out.orders.push(OrderUpdate {
                    market: market.to_lowercase(),
                    id: id.parse().context("order id")?,
                    price: price.parse().context("order price")?,
                    is_buy: match side {
                        "b" => true,
                        "a" => false,
                        _ => bail!("order side {side:?}"),
                    },
                    size: size.to_string(),
                    ..Default::default()
                });
            }
            _ => bail!("snapshot row {row:?} is neither a market nor an order"),
        }
    }
    Ok(out)
}

#[substreams::handlers::store]
fn store_markets(
    snapshot: Snapshot,
    markets: BlockTransactionProtocolComponents,
    store: StoreSetString,
) {
    for m in &snapshot.markets {
        store.set(0, &m.market, &m.params);
    }
    for c in markets
        .tx_components
        .iter()
        .flat_map(|t| t.components.iter())
    {
        let get = |n: &str| {
            c.static_att
                .iter()
                .find(|a| a.name == n)
                .map(|a| BigInt::from_signed_bytes_be(&a.value))
        };
        let m = Market::new(
            get("price_precision").unwrap().to_u64() as u128,
            get("size_precision")
                .unwrap()
                .to_string()
                .parse()
                .unwrap(),
            get("base_decimals").map(|v| v.to_u64() as u32),
            get("quote_decimals").map(|v| v.to_u64() as u32),
            c.tokens[0].clone(),
            c.tokens[1].clone(),
        );
        store.set(0, &c.id, &m.encode());
    }
}

#[substreams::handlers::map]
fn map_events(
    block: eth::Block,
    markets: StoreGetString,
) -> Result<Events, substreams::errors::Error> {
    let mut events = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(l, _)| l) {
            let market = hex0x(&log.address);
            if markets.get_last(&market).is_none() {
                continue;
            }
            let Some(ev) = log_event(log, market, tx) else {
                continue;
            };
            events.push(ev);
        }
    }
    events.sort_by_key(|e| e.ordinal);
    Ok(Events { events })
}

/// The order book event `log` of `market` carries, if any.
fn log_event(log: &eth::Log, market: String, tx: &eth::TransactionTrace) -> Option<Event> {
    let base = Event { market, ordinal: log.ordinal, tx: Some(tx.into()), ..Default::default() };
    Some(if let Some(e) = ob::OrderCreated::match_and_decode(log) {
        Event {
            kind: 0,
            order_id: e.order_id.to_u64(),
            price: e.price.to_u64() as u32,
            size: e.size.to_string(),
            is_buy: e.is_buy,
            ..base
        }
    } else if let Some(e) = ob::FlipOrderCreated::match_and_decode(log) {
        Event {
            kind: 0,
            order_id: e.order_id.to_u64(),
            price: e.price.to_u64() as u32,
            size: e.size.to_string(),
            is_buy: e.is_buy,
            ..base
        }
    } else if let Some(e) = ob::FlippedOrderCreated::match_and_decode(log) {
        Event {
            kind: 0,
            order_id: e.order_id.to_u64(),
            price: e.price.to_u64() as u32,
            size: e.size.to_string(),
            is_buy: e.is_buy,
            ..base
        }
    } else if let Some(e) = ob::OrderCanceled::match_and_decode(log) {
        Event {
            kind: 1,
            order_id: e.order_id.to_u64(),
            price: e.price.to_u64() as u32,
            size: e.size.to_string(),
            is_buy: e.is_buy,
            ..base
        }
    } else if let Some(e) = ob::FlipOrderUpdated::match_and_decode(log) {
        Event { kind: 2, order_id: e.order_id.to_u64(), size: e.size.to_string(), ..base }
    } else if let Some(e) = ob::Trade::match_and_decode(log) {
        Event {
            kind: 3,
            order_id: e.order_id.to_u64(),
            is_buy: e.is_buy,
            size: e.updated_size.to_string(),
            price_1e18: e.price.to_string(),
            filled: e.filled_size.to_string(),
            ..base
        }
    } else {
        return None;
    })
}

/// Decimal store/event value; empty = absent (0).
fn u(s: &str) -> u128 {
    if s.is_empty() {
        0
    } else {
        s.parse().expect("decimal u128")
    }
}

fn book_event(e: &Event) -> BookEvent {
    match e.kind {
        0 => BookEvent::Created {
            id: e.order_id,
            price: e.price,
            size: u(&e.size),
            is_buy: e.is_buy,
        },
        1 => BookEvent::OrderCanceled {
            id: e.order_id,
            price: e.price,
            size: u(&e.size),
            is_buy: e.is_buy,
        },
        2 => BookEvent::FlipOrderUpdated { id: e.order_id, size: u(&e.size) },
        _ => BookEvent::Trade {
            id: e.order_id,
            taker_buy: e.is_buy,
            price_1e18: u(&e.price_1e18),
            updated_size: u(&e.size),
            filled: u(&e.filled),
        },
    }
}

/// Trailing `:` so `delete_prefix` of one order never matches a longer id.
fn order_key(e: &Event) -> String {
    key_of(&e.market, e.order_id)
}

fn key_of(market: &str, id: u64) -> String {
    format!("{market}:{id}:")
}

/// Live order sizes; a key is deleted when its order leaves the book, so the store holds resting
/// orders only.
#[substreams::handlers::store]
fn store_order_sizes(snapshot: Snapshot, events: Events, store: StoreSetBigInt) {
    for o in &snapshot.orders {
        store.set(0, key_of(&o.market, o.id), &BigInt::from_str(&o.size).unwrap());
    }
    for e in &events.events {
        match book_event(e).order_size() {
            Some((_, 0)) => store.delete_prefix(e.ordinal as i64, &order_key(e)),
            Some((_, size)) => store.set(e.ordinal, order_key(e), &big(size)),
            None => {}
        }
    }
}

/// Price and side of resting orders (`FlipOrderUpdated` and `Trade` carry no tick), deleted with
/// the order.
#[substreams::handlers::store]
fn store_order_meta(snapshot: Snapshot, events: Events, store: StoreSetIfNotExistsString) {
    for o in &snapshot.orders {
        store.set_if_not_exists(0, key_of(&o.market, o.id), &format!("{}:{}", o.price, o.is_buy));
    }
    for e in &events.events {
        if e.kind == 0 {
            store.set_if_not_exists(e.ordinal, order_key(e), &format!("{}:{}", e.price, e.is_buy));
        } else if matches!(book_event(e).order_size(), Some((_, 0))) {
            store.delete_prefix(e.ordinal as i64, &order_key(e));
        }
    }
}

fn big(x: u128) -> BigInt {
    BigInt::from_str(&x.to_string()).expect("u128 is a valid integer")
}

#[substreams::handlers::map]
fn map_level_deltas(
    events: Events,
    sizes: StoreDeltas,
    meta: StoreGetString,
    markets: StoreGetString,
) -> Result<LevelDeltas, substreams::errors::Error> {
    // size before each set, by (order key, ordinal)
    let before: HashMap<(String, u64), u128> = sizes
        .deltas
        .iter()
        .map(|d| {
            let old = String::from_utf8_lossy(&d.old_value);
            ((d.key.clone(), d.ordinal), u(&old))
        })
        .collect();
    level_deltas(
        &events,
        |key, ordinal| {
            before
                .get(&(key.to_string(), ordinal))
                .copied()
                .unwrap_or(0)
        },
        |key, ordinal| meta.get_at(ordinal, key),
        |market| Market::decode(&markets.get_last(market).unwrap()).pp,
    )
}

/// Level deltas and order updates of `events`. `size_before(key, ordinal)` is an order's size
/// just before the event at `ordinal`, `meta_at(key, ordinal)` its `price:is_buy` as of
/// `ordinal`, and `pp(market)` the market's price precision.
///
/// Errors on an event that resizes an order the stores do not know: every resting order is
/// either created in the stream or seeded by the snapshot, so this means a stream started after
/// the market's first order without a snapshot.
fn level_deltas(
    events: &Events,
    size_before: impl Fn(&str, u64) -> u128,
    meta_at: impl Fn(&str, u64) -> Option<String>,
    pp: impl Fn(&str) -> u128,
) -> Result<LevelDeltas, substreams::errors::Error> {
    let mut out = LevelDeltas::default();
    for e in &events.events {
        let ev = book_event(e);
        let key = order_key(e);
        // the order as it stood just before this event (a delete at this ordinal is the event's
        // own)
        let known = || -> Option<(u32, bool)> {
            let m = meta_at(&key, e.ordinal.saturating_sub(1))?;
            let (price, is_buy) = m
                .split_once(':')
                .expect("meta is price:is_buy");
            Some((
                price
                    .parse()
                    .expect("meta price is a u32"),
                is_buy == "true",
            ))
        };
        let order = match ev {
            BookEvent::FlipOrderUpdated { .. } | BookEvent::Trade { id: 1.., .. } => {
                known().map(|(price, is_buy)| (price, is_buy, size_before(&key, e.ordinal)))
            }
            _ => None,
        };
        if let Some(d) = ev
            .delta(pp(&e.market), order)
            .map_err(|x| anyhow!("market {} order {}: {x}", e.market, e.order_id))?
        {
            let delta = BigInt::from_str(&d.add.to_string()).unwrap() -
                BigInt::from_str(&d.sub.to_string()).unwrap();
            out.deltas.push(LevelDelta {
                market: e.market.clone(),
                ordinal: e.ordinal,
                tx: e.tx.clone(),
                is_buy: d.is_buy,
                price: d.price,
                delta: delta.to_string(),
            });
        }
        if let Some((id, size)) = ev.order_size() {
            let (price, is_buy) = match ev {
                BookEvent::Created { price, is_buy, .. } => (price, is_buy),
                _ => known()
                    .ok_or_else(|| anyhow!("market {} event on unknown order {id}", e.market))?,
            };
            out.orders.push(OrderUpdate {
                market: e.market.clone(),
                ordinal: e.ordinal,
                tx: e.tx.clone(),
                id,
                price,
                is_buy,
                size: size.to_string(),
            });
        }
    }
    Ok(out)
}

/// Book depth as component balances (TVL only): asks in base, bids in quote.
#[substreams::handlers::map]
fn map_balance_deltas(
    deltas: LevelDeltas,
    markets: StoreGetString,
) -> Result<BlockBalanceDeltas, substreams::errors::Error> {
    Ok(balance_deltas(&deltas, |market| Market::decode(&markets.get_last(market).unwrap())))
}

fn balance_deltas(deltas: &LevelDeltas, market: impl Fn(&str) -> Market) -> BlockBalanceDeltas {
    let ten = |d: u32| BigInt::from(10).pow(d);
    let balance_deltas = deltas
        .deltas
        .iter()
        .filter_map(|d| {
            let m = market(&d.market);
            if m.base_dec == u32::MAX || m.quote_dec == u32::MAX {
                return None;
            }
            let size = BigInt::from_str(&d.delta).unwrap();
            let sp = BigInt::from_str(&m.sp.to_string()).unwrap();
            let (token, amount) = if d.is_buy {
                let pp = BigInt::from_str(&m.pp.to_string()).unwrap();
                (m.quote.clone(), size * BigInt::from(d.price) * ten(m.quote_dec) / (sp * pp))
            } else {
                (m.base.clone(), size * ten(m.base_dec) / sp)
            };
            Some(BalanceDelta {
                ord: d.ordinal,
                tx: d.tx.clone(),
                token,
                delta: amount.to_signed_bytes_be(),
                component_id: d.market.clone().into_bytes(),
            })
        })
        .collect();
    BlockBalanceDeltas { balance_deltas }
}

/// Vault, state and fee attributes from the market's own storage writes in `tx`.
fn storage_attrs(
    tx: &eth::TransactionTrace,
    markets: &StoreGetString,
) -> HashMap<String, Vec<Attribute>> {
    // (address, slot) -> (value before the tx, value after it), in storage-change order
    let mut changes: Vec<&eth::StorageChange> = tx
        .calls
        .iter()
        .filter(|c| !c.state_reverted)
        .flat_map(|c| c.storage_changes.iter())
        .collect();
    changes.sort_by_key(|sc| sc.ordinal);
    type Slot = (Vec<u8>, u8);
    let mut last: HashMap<Slot, (Vec<u8>, Vec<u8>)> = HashMap::new();
    for sc in changes {
        if let Some(s) = slot_of(&sc.key) {
            last.entry((sc.address.clone(), s))
                .or_insert_with(|| (sc.old_value.clone(), Vec::new()))
                .1 = sc.new_value.clone();
        }
    }
    let mut out: HashMap<String, Vec<Attribute>> = HashMap::new();
    for ((addr, s), (before, w)) in last {
        let id = hex0x(&addr);
        if markets.get_last(&id).is_none() {
            continue;
        }
        // slot 61 also holds the order id counter: emit only when the state byte moves
        if s == slot::STATE && field(&word(&w), 40, 8) == field(&word(&before), 40, 8) {
            continue;
        }
        let attrs: Vec<Attribute> = slot_attributes(s, &word(&w))
            .into_iter()
            .map(|(name, value)| attr(name, value))
            .collect();
        if attrs.is_empty() {
            continue;
        }
        out.entry(id).or_default().extend(attrs);
    }
    out
}

#[allow(clippy::too_many_arguments)]
#[substreams::handlers::map]
fn map_protocol_changes(
    block: eth::Block,
    new_markets: BlockTransactionProtocolComponents,
    markets: StoreGetString,
    level_deltas: LevelDeltas,
    balance_deltas: BlockBalanceDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut txs: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    for t in &new_markets.tx_components {
        let tx = t.tx.as_ref().unwrap();
        let b = txs
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        for c in &t.components {
            b.add_protocol_component(c);
            // mutable parameters start at their registration values; storage writes update them
            if let Some(ev) = block
                .transactions()
                .filter(|x| x.index as u64 == tx.index)
                .flat_map(|x| x.logs_with_calls().map(|(l, _)| l))
                .filter_map(MarketRegistered::match_and_decode)
                .find(|ev| hex0x(&ev.market) == c.id)
            {
                b.add_entity_change(&EntityChanges {
                    component_id: c.id.clone(),
                    attributes: vec![
                        attr("taker_fee_bps", ev.taker_fee_bps.to_signed_bytes_be()),
                        attr("maker_fee_bps", ev.maker_fee_bps.to_signed_bytes_be()),
                        attr("vault_spread", ev.kuru_amm_spread.to_signed_bytes_be()),
                    ],
                });
            }
        }
    }

    for tx in block.transactions() {
        for (id, attributes) in storage_attrs(tx, &markets) {
            let t: Transaction = tx.into();
            txs.entry(t.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&t))
                .add_entity_change(&EntityChanges { component_id: id, attributes });
        }
    }

    add_book_changes(&mut txs, level_deltas, balance_deltas);

    let mut changes: Vec<_> = txs
        .into_values()
        .filter_map(|b| b.build())
        .collect();
    changes.sort_by_key(|c| c.tx.as_ref().map(|t| t.index));
    Ok(BlockChanges { block: Some((&block).into()), changes, ..Default::default() })
}

/// Levels `a/<price>`, `b/<price>` and balances as deltas, resting orders as `o/<id>` (deleted
/// when the order leaves the book), so the output does not depend on the block the stream starts
/// at.
fn add_book_changes(
    txs: &mut HashMap<u64, TransactionChangesBuilder>,
    level_deltas: LevelDeltas,
    balance_deltas: BlockBalanceDeltas,
) {
    let mut entry = |tx: &Transaction, component_id: &str, attribute: Attribute| {
        txs.entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx))
            .add_entity_change(&EntityChanges {
                component_id: component_id.to_string(),
                attributes: vec![attribute],
            });
    };
    for d in &level_deltas.deltas {
        let name = format!("{}/{}", if d.is_buy { "b" } else { "a" }, d.price);
        let value = BigInt::from_str(&d.delta).unwrap();
        let change = ChangeType::Delta.into();
        entry(
            d.tx.as_ref().unwrap(),
            &d.market,
            Attribute { name, value: value.to_signed_bytes_be(), change },
        );
    }
    for o in &level_deltas.orders {
        let size = u(&o.size);
        let (value, change) = if size == 0 {
            (vec![], ChangeType::Deletion)
        } else {
            (encode_order((o.price, o.is_buy, size)).to_vec(), ChangeType::Update)
        };
        entry(
            o.tx.as_ref().unwrap(),
            &o.market,
            Attribute { name: format!("o/{}", o.id), value, change: change.into() },
        );
    }
    for d in balance_deltas.balance_deltas {
        let tx = d.tx.unwrap();
        txs.entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_balance_change(&BalanceChange {
                token: d.token,
                balance: d.delta,
                component_id: d.component_id,
                change: ChangeType::Delta.into(),
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_roundtrip() {
        let m = Market {
            pp: 100_000_000,
            sp: 10_000_000_000,
            base_dec: 18,
            quote_dec: 6,
            base: vec![0; 20],
            quote: vec![1; 20],
        };
        let d = Market::decode(&m.encode());
        assert_eq!((d.pp, d.sp, d.base_dec, d.quote_dec, d.quote), (m.pp, m.sp, 18, 6, m.quote));
    }
}
