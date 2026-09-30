//! Kuru (Monad CLOB) book rules: how each market event moves the price levels.
//!
//! One implementation for both consumers: the `monad-kuru` substreams (stores, wasm) and
//! tycho-simulation's resync/replay (`LevelBook`). Levels are sums of order sizes per price
//! (sizePrecision units); prices are `uint32` in pricePrecision units.
#![no_std]
extern crate alloc;

use alloc::{collections::BTreeMap, vec, vec::Vec};

/// Market storage slots (Kuru `AbstractAMM` + `OrderBook` layout, verified on chain 09-29).
pub mod slot {
    pub const VAULT_BEST_BID: u8 = 0;
    pub const VAULT_BID_PARTIAL: u8 = 1; // high 96 bits; low 160 = vault address
    pub const VAULT_BEST_ASK: u8 = 2;
    pub const VAULT_ASK: u8 = 3; // low 96 ask partial, next 96 ask size
    pub const VAULT_BID: u8 = 4; // low 96 bid size, next 96 spread
    /// `mapping(uint256 price => PricePoint)` of bids and asks; a price point packs the head
    /// order id (low 40 bits) and the tail (next 40).
    pub const BUY_PRICE_POINTS: u8 = 51;
    pub const SELL_PRICE_POINTS: u8 = 52;
    pub const STATE: u8 = 61; // orderIdCounter u40 | marketState u8 | sizePrecision u96 | pricePrecision u32
    pub const TAKER_FEE: u8 = 62;
    pub const MAKER_FEE: u8 = 63;
    pub const BASE_DECIMALS: u8 = 64;
    pub const QUOTE_DECIMALS: u8 = 66;
    /// Slots whose value the market's attributes mirror.
    pub const ATTRIBUTES: [u8; 8] = [
        VAULT_BEST_BID,
        VAULT_BID_PARTIAL,
        VAULT_BEST_ASK,
        VAULT_ASK,
        VAULT_BID,
        STATE,
        TAKER_FEE,
        MAKER_FEE,
    ];
}

/// `bits` bits of the big-endian `word` starting at bit `from`, as minimal big-endian bytes
/// (`[0]` for zero).
pub fn field(word: &[u8; 32], from: usize, bits: usize) -> Vec<u8> {
    let mut out = [0u8; 32];
    for bit in 0..bits.min(256 - from) {
        let src = from + bit;
        if word[31 - src / 8] >> (src % 8) & 1 == 1 {
            out[31 - bit / 8] |= 1 << (bit % 8);
        }
    }
    let first = out
        .iter()
        .position(|b| *b != 0)
        .unwrap_or(31);
    out[first..].to_vec()
}

/// The attributes a write of `word` to market storage `slot` sets, or none for a slot no
/// attribute mirrors.
pub fn slot_attributes(slot: u8, word: &[u8; 32]) -> Vec<(&'static str, Vec<u8>)> {
    match slot {
        slot::VAULT_BEST_BID => vec![("vault_best_bid", field(word, 0, 256))],
        slot::VAULT_BID_PARTIAL => vec![("vault_bid_partial", field(word, 160, 96))],
        slot::VAULT_BEST_ASK => vec![("vault_best_ask", field(word, 0, 256))],
        slot::VAULT_ASK => {
            vec![("vault_ask_partial", field(word, 0, 96)), ("vault_ask_size", field(word, 96, 96))]
        }
        slot::VAULT_BID => {
            vec![("vault_bid_size", field(word, 0, 96)), ("vault_spread", field(word, 96, 96))]
        }
        slot::STATE => vec![("active", vec![u8::from(field(word, 40, 8) == [0])])],
        slot::TAKER_FEE => vec![("taker_fee_bps", field(word, 0, 256))],
        slot::MAKER_FEE => vec![("maker_fee_bps", field(word, 0, 256))],
        _ => vec![],
    }
}

/// Market events, already decoded (ABI decoding is per consumer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// OrderCreated, FlipOrderCreated, FlippedOrderCreated
    Created {
        id: u64,
        price: u32,
        size: u128,
        is_buy: bool,
    },
    OrderCanceled {
        id: u64,
        price: u32,
        size: u128,
        is_buy: bool,
    },
    /// Carries only the new size of `id` (a flip order's twin).
    FlipOrderUpdated {
        id: u64,
        size: u128,
    },
    /// `taker_buy` is `Trade.isBuy`; `price_1e18` is `Trade.price`. `id` 0 = vault fill.
    Trade {
        id: u64,
        taker_buy: bool,
        price_1e18: u128,
        updated_size: u128,
        filled: u128,
    },
    MarketState {
        active: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delta {
    pub is_buy: bool,
    pub price: u32,
    pub add: u128,
    pub sub: u128,
}

/// A live order as the rules need it: (price, is_buy, size).
pub type OrderRef = (u32, bool, u128);

const VPP: u128 = 1_000_000_000_000_000_000;

/// Value of a resting order's `o/<id>` attribute: price (4 bytes), is_buy (1), size (12),
/// big-endian.
pub fn encode_order((price, is_buy, size): OrderRef) -> [u8; 17] {
    let mut out = [0u8; 17];
    out[..4].copy_from_slice(&price.to_be_bytes());
    out[4] = u8::from(is_buy);
    out[5..].copy_from_slice(&size.to_be_bytes()[4..]);
    out
}

/// Inverse of [`encode_order`]; accepts the value with leading zero bytes stripped.
pub fn decode_order(value: &[u8]) -> Result<OrderRef, &'static str> {
    if value.len() > 17 {
        return Err("order attribute longer than 17 bytes");
    }
    let mut full = [0u8; 17];
    full[17 - value.len()..].copy_from_slice(value);
    let mut size = [0u8; 16];
    size[4..].copy_from_slice(&full[5..]);
    let price = u32::from_be_bytes(full[..4].try_into().expect("4 bytes"));
    Ok((price, full[4] == 1, u128::from_be_bytes(size)))
}

impl Event {
    /// The order whose size this event sets, and the size (0 = gone).
    pub fn order_size(&self) -> Option<(u64, u128)> {
        match *self {
            Event::Created { id, size, .. } => Some((id, size)),
            Event::OrderCanceled { id, .. } => Some((id, 0)),
            Event::FlipOrderUpdated { id, size } => Some((id, size)),
            Event::Trade { id, updated_size, .. } if id != 0 => Some((id, updated_size)),
            _ => None,
        }
    }

    /// Level delta of this event. `order` = the order the event names, as it stood just before it:
    /// required for `FlipOrderUpdated` (it carries only the new size), used for `Trade` when known.
    pub fn delta(
        &self,
        price_precision: u128,
        order: Option<OrderRef>,
    ) -> Result<Option<Delta>, &'static str> {
        Ok(match *self {
            Event::Created { price, size, is_buy, .. } => {
                Some(Delta { is_buy, price, add: size, sub: 0 })
            }
            Event::OrderCanceled { price, size, is_buy, .. } => {
                Some(Delta { is_buy, price, add: 0, sub: size })
            }
            Event::FlipOrderUpdated { size, .. } => {
                let (price, is_buy, old) = order.ok_or("FlipOrderUpdated on an unknown order")?;
                Some(if size >= old {
                    Delta { is_buy, price, add: size - old, sub: 0 }
                } else {
                    Delta { is_buy, price, add: 0, sub: old - size }
                })
            }
            // Vault fills move vault storage, not levels.
            Event::Trade { id: 0, .. } => None,
            Event::Trade { taker_buy, price_1e18, filled, .. } => {
                // the maker order's own tick when known; else from `Trade.price` = tick * 1e18 /
                // pricePrecision, exact for the power-of-ten precisions the Router enforces
                let price = match order {
                    Some((price, _, _)) => price,
                    None => {
                        let scaled = price_1e18
                            .checked_mul(price_precision)
                            .ok_or("Trade price overflow")?;
                        if scaled % VPP != 0 {
                            return Err("Trade price is not on a tick");
                        }
                        u32::try_from(scaled / VPP).map_err(|_| "Trade price > uint32")?
                    }
                };
                Some(Delta { is_buy: !taker_buy, price, add: 0, sub: filled })
            }
            Event::MarketState { .. } => None,
        })
    }
}

/// Levels + live orders, replayed in memory (resync path; the substreams keeps the same in stores).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelBook {
    pub bids: BTreeMap<u32, u128>,
    pub asks: BTreeMap<u32, u128>,
    /// live order sizes, needed by `FlipOrderUpdated`; orders older than the snapshot must be
    /// seeded from `s_orders(id)`
    pub orders: BTreeMap<u64, OrderRef>,
    pub active: Option<bool>,
    pub price_precision: u128,
}

impl LevelBook {
    pub fn apply(&mut self, ev: &Event) -> Result<(), &'static str> {
        let order = match ev {
            Event::FlipOrderUpdated { id, .. } => Some(
                *self
                    .orders
                    .get(id)
                    .ok_or("FlipOrderUpdated on an unknown order")?,
            ),
            Event::Trade { id, .. } => self.orders.get(id).copied(),
            _ => None,
        };
        if let Some(d) = ev.delta(self.price_precision, order)? {
            let side = if d.is_buy { &mut self.bids } else { &mut self.asks };
            let cur = side.get(&d.price).copied().unwrap_or(0) + d.add;
            let new = cur
                .checked_sub(d.sub)
                .ok_or("level underflow")?;
            if new == 0 {
                side.remove(&d.price);
            } else {
                side.insert(d.price, new);
            }
        }
        match *ev {
            Event::Created { id, price, size, is_buy } => {
                self.orders
                    .insert(id, (price, is_buy, size));
            }
            Event::MarketState { active } => self.active = Some(active),
            _ => {
                if let Some((id, size)) = ev.order_size() {
                    if size == 0 {
                        self.orders.remove(&id);
                    } else if let Some(o) = self.orders.get_mut(&id) {
                        o.2 = size;
                    }
                }
            }
        }
        Ok(())
    }

    /// Ids `FlipOrderUpdated` names that no `Created` in `evs` introduces: seed these first.
    pub fn unseeded(evs: &[Event]) -> Vec<u64> {
        let mut born = alloc::collections::BTreeSet::new();
        let mut need = Vec::new();
        for ev in evs {
            match *ev {
                Event::Created { id, .. } => {
                    born.insert(id);
                }
                Event::FlipOrderUpdated { id, .. }
                    if !born.contains(&id) && !need.contains(&id) =>
                {
                    need.push(id)
                }
                _ => {}
            }
        }
        need
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_trade_flip_cancel() {
        let mut b = LevelBook { price_precision: VPP, ..Default::default() };
        b.apply(&Event::Created { id: 1, price: 5, size: 10, is_buy: false })
            .unwrap();
        b.apply(&Event::Trade {
            id: 1,
            taker_buy: true,
            price_1e18: 5,
            updated_size: 6,
            filled: 4,
        })
        .unwrap();
        assert_eq!(b.asks.get(&5), Some(&6));
        b.apply(&Event::FlipOrderUpdated { id: 1, size: 9 })
            .unwrap();
        assert_eq!(b.asks.get(&5), Some(&9));
        b.apply(&Event::Trade {
            id: 0,
            taker_buy: true,
            price_1e18: 7,
            updated_size: 0,
            filled: 3,
        })
        .unwrap();
        b.apply(&Event::OrderCanceled { id: 1, price: 5, size: 9, is_buy: false })
            .unwrap();
        assert!(b.asks.is_empty() && b.orders.is_empty());
        assert!(b
            .apply(&Event::FlipOrderUpdated { id: 2, size: 1 })
            .is_err());
        assert_eq!(
            LevelBook::unseeded(&[
                Event::FlipOrderUpdated { id: 3, size: 1 },
                Event::Created { id: 4, price: 1, size: 1, is_buy: true },
                Event::FlipOrderUpdated { id: 4, size: 2 }
            ]),
            [3]
        );
    }

    #[test]
    fn slot_fields_match_chain() {
        // MON/USDC market slot 61 and slot 4, read 09-29
        let hex = |s: &str| -> [u8; 32] {
            let mut w = [0u8; 32];
            for (i, b) in w.iter_mut().enumerate() {
                *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
            }
            w
        };
        let w61 = hex("0000000000000000000005f5e1000000000000000002540be400000006c336fc");
        assert_eq!(field(&w61, 40, 8), [0]); // state 0 = active
        assert_eq!(field(&w61, 48, 96), 10_000_000_000u64.to_be_bytes()[3..]);
        assert_eq!(slot_attributes(slot::STATE, &w61), [("active", alloc::vec![1])]);
        let w4 = hex("000000000000000000000000000000000000001e000000000000000000000000");
        assert_eq!(field(&w4, 96, 96), [30]);
        assert_eq!(field(&w4, 0, 96), [0]);
        assert_eq!(field(&w61, 0, 256)[..3], [0x05, 0xf5, 0xe1]);
    }

    #[test]
    fn order_attribute_roundtrip() {
        let order = (2_681_200, true, (1u128 << 95) + 7);
        assert_eq!(decode_order(&encode_order(order)).unwrap(), order);
        let small = (0, false, 5);
        assert_eq!(decode_order(&encode_order(small)[16..]).unwrap(), small);
        assert!(decode_order(&[0; 18]).is_err());
    }

    #[test]
    fn trade_prefers_the_order_tick() {
        // pp 3 does not divide 1e18: the price is only recoverable from the order itself
        let t =
            Event::Trade { id: 9, taker_buy: true, price_1e18: 333, updated_size: 0, filled: 2 };
        assert!(t.delta(3, None).is_err());
        assert_eq!(
            t.delta(3, Some((7, false, 2))).unwrap(),
            Some(Delta { is_buy: false, price: 7, add: 0, sub: 2 })
        );
    }

    #[test]
    fn trade_price_back_to_ticks() {
        // pp 1e8: Trade.price = price * 1e18 / 1e8
        let d = Event::Trade {
            id: 9,
            taker_buy: false,
            price_1e18: 2_890_900 * 10_000_000_000,
            updated_size: 0,
            filled: 1,
        }
        .delta(100_000_000, None)
        .unwrap()
        .unwrap();
        assert_eq!(d, Delta { is_buy: true, price: 2_890_900, add: 0, sub: 1 });
    }
}
