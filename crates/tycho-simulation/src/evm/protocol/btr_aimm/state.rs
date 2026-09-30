//! BTR AIMM pool state = the raw storage words its quote reads (`Asset` words, `MarkStoreP8` P/R
//! tier words, class table, curve blobs). Quotes run through `btr-core`; the gate that turns a
//! store word into a usable mark, the curve decode and the pool builder are `btr_core::replica`
//! (shared with the keepers), without the keepers' pending-push overlay: the state is the chain's,
//! as of `ts`.
//!
//! Simulation only: settlement goes through the pool's `swapCoop` native leg, there is no
//! `tycho-execution` encoder for this protocol.
use std::{any::Any, collections::BTreeMap};

use alloy::primitives::B256;
use btr_core::{
    fixed::U256 as CU,
    pricing::Endpoint,
    replica::{self, Asset},
    route::NamedPool,
    storage::{self as st, p8},
};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use super::decoder::apply_attribute;

/// `get_limits` searches inputs up to this many times the larger of the two raw reserves. Fills
/// stop at the coverage wall well inside it; the bound only ends the search on a pool that never
/// clamps (and on a decimals mix, where raw reserves are a loose scale).
const LIMIT_CAP_X: u128 = 16;
/// One `swap` (oracle-priced, no tick walk): rough, the arb path prices gas itself.
const SWAP_GAS: u64 = 250_000;

/// Which `PricingLib` entry the quote answers for, i.e. which flags open a leg.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// `swapExactIn` for a caller with no perms: what Fynd routes. `SWAP_GATED` closes a leg.
    #[default]
    Public,
    /// `swapCoop`, what a `CoopArb` settles: a leg needs `COOP_ENABLED`, `SWAP_GATED` is ignored.
    /// The pricing is the same; only the gate differs (the coop fee re-split is not modelled).
    Coop,
}

/// One listed token's `Asset` words plus its oracle lane's tier words.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leg {
    /// reserves | liabilities
    pub w0: B256,
    /// risk config + oracle wiring
    pub w2: B256,
    /// `MarkStoreP8` P tier word of the leg's lane
    pub p: B256,
    /// `MarkStoreP8` R tier word of the leg's lane
    pub r: B256,
}

impl Leg {
    /// The `Asset` words as `btr_core::replica` reads them (word 1 is not read by the quote).
    fn asset(&self) -> Asset {
        Asset([self.w0.0, [0; 32], self.w2.0])
    }
    fn endpoint(&self) -> Endpoint {
        self.asset().endpoint()
    }
    /// `PoolIOLib.settle`'s move on this endpoint; `None` = the pool's checked math reverts on it.
    fn settle(&mut self, res_add: u128, res_sub: u128, liab_add: u128) -> Option<()> {
        let mut a = self.asset();
        a.settle(res_add, res_sub, liab_add)?;
        self.w0 = B256::from(a.0[0]);
        Some(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtrAimmState {
    /// Pool storage slot 0.
    pub slot0: B256,
    /// `MarkStoreP8.classes()`: laneCls | clsA | clsB.
    pub classes: [B256; 3],
    /// Block time the marks are gated against.
    pub ts: u64,
    /// Which entry gates the legs; set by the component's static attribute `mode` (`coop` or
    /// `public`, default `public`).
    pub mode: Mode,
    pub legs: BTreeMap<Bytes, Leg>,
    /// curve id -> `eth_getCode` blob.
    pub curves: BTreeMap<u16, Bytes>,
}

/// Amounts of one settled swap, native raw units.
struct Swapped {
    out: u128,
    lp_fee: u128,
    proto_fee: u128,
}

/// The largest `a <= cap` with `fills(a)`, by doubling then bisection; `fills` is monotone
/// (true up to the wall, false past it). A size that fills AT the cap ends the search there.
/// `None` = nothing fills.
fn max_fill(cap: u128, fills: impl Fn(u128) -> bool) -> Option<u128> {
    let (mut lo, mut a) = (0u128, 1u128);
    let mut hi = loop {
        let x = a.min(cap);
        if fills(x) {
            lo = x;
            if x == cap {
                return Some(cap);
            }
        } else if lo != 0 {
            break x;
        } else if x == cap {
            return None;
        }
        a = a.saturating_mul(2);
    };
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if fills(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(lo)
}

type R<T> = Result<T, SimulationError>;

fn invalid(what: impl Into<String>) -> SimulationError {
    SimulationError::InvalidInput(format!("BtrAimm: {}", what.into()), None)
}

fn key(t: &Bytes) -> String {
    format!("{t:#x}")
}

impl BtrAimmState {
    /// A decodable snapshot: base known, class table and at least one curve present.
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.classes.iter().all(|c| c.is_zero()) {
            return Err("missing attribute classes".into());
        }
        let base = self.base();
        if base.iter().all(|b| *b == 0) {
            return Err("missing attribute slot0".into());
        }
        if !self.legs.contains_key(&base) {
            return Err("base token has no leg".into());
        }
        if self.curves.is_empty() {
            return Err("missing attribute curve/<id>".into());
        }
        Ok(())
    }

    fn base(&self) -> Bytes {
        Bytes::from(
            st::decode_pool_slot0(&self.slot0.0)
                .base_token
                .to_vec(),
        )
    }

    fn classes(&self) -> p8::Classes {
        p8::Classes {
            lane_cls: self.classes[0].0,
            cls_a: self.classes[1].0,
            cls_b: self.classes[2].0,
        }
    }

    fn leg(&self, t: &Bytes) -> R<&Leg> {
        self.legs
            .get(t)
            .ok_or_else(|| invalid(format!("token {t} is not listed")))
    }

    /// The leg's P-tier mark as the pool prices it, gated at `ts`.
    fn mark(&self, leg: &Leg, cls: &p8::Classes) -> Option<replica::Gate> {
        let (lane, internal, uoa, band) = st::decode_leg_oracle(&leg.w2.0);
        let sw = p8::store_word(&leg.p.0, &leg.r.0, lane, cls);
        replica::gated(&st::decode_store_word(&sw, internal, uoa, band).primary, self.ts)
    }

    /// Whether the leg is open to the entry this state's [`Mode`] answers for.
    fn open(&self, a: &Asset) -> bool {
        match self.mode {
            Mode::Public => a.swappable(),
            Mode::Coop => a.coop_open(),
        }
    }

    /// The pool as `btr-core` routes it, restricted to `base` and the spokes `tin`/`tout` name.
    /// Errors when either token is closed to the [`Mode`]'s entry (halted, swaps off, gated or
    /// coop off) or its mark or curve does not gate: the pool reverts on it.
    fn named_pool(&self, tin: &Bytes, tout: &Bytes) -> R<NamedPool> {
        let base_addr = self.base();
        let cls = self.classes();
        let base = self.leg(&base_addr)?;
        let base_mark = self
            .mark(base, &cls)
            .ok_or_else(|| invalid("base mark is not live"))?
            .mark_1e18;
        let proto_share_pct = st::decode_pool_slot0(&self.slot0.0).proto_share_pct;
        let mut spokes: Vec<btr_core::route::Spoke> = Vec::new();
        for t in [tin, tout] {
            if !self.open(&self.leg(t)?.asset()) {
                return Err(invalid(format!("token {t} is closed to {:?} swaps", self.mode)));
            }
            if *t == base_addr || spokes.iter().any(|s| s.token == key(t)) {
                continue;
            }
            let leg = self.leg(t)?;
            let gate = self
                .mark(leg, &cls)
                .ok_or_else(|| invalid(format!("mark of {t} is not live")))?;
            let asset = leg.asset();
            let curve = self
                .curves
                .get(&asset.curve_id())
                .and_then(|b| replica::curve(b.as_ref()))
                .ok_or_else(|| invalid(format!("curve of {t} is missing or malformed")))?;
            spokes.push(
                replica::spoke(key(t), &asset, gate, base_mark, curve, proto_share_pct)
                    .ok_or_else(|| invalid(format!("mark of {t} overflows")))?,
            );
        }
        Ok(replica::named_pool(&key(&base_addr), &base.asset(), spokes))
    }

    /// `anchorPathQuoteLp` for `amt` of `tin` (native raw) on `p`, the settling hop's amounts.
    /// Errors when the pool refuses or clamps at the coverage wall. A spoke-to-spoke cross is
    /// `btr-core`'s per-hop composition: approximate, the chain settles one path spread.
    fn quote_on(p: &NamedPool, tin: &Bytes, tout: &Bytes, amt: u128) -> R<Swapped> {
        let h = replica::quote_hop(p, &key(tin), &key(tout), amt)
            .ok_or_else(|| invalid("unquotable or clamped at the coverage wall"))?;
        let f = |x: CU| {
            x.to_u128()
                .ok_or_else(|| invalid("amount exceeds u128"))
        };
        Ok(Swapped { out: f(h.amount_out)?, lp_fee: f(h.lp_fee)?, proto_fee: f(h.proto_fee)? })
    }

    fn quote(&self, tin: &Bytes, tout: &Bytes, amt: u128) -> R<Swapped> {
        if tin == tout {
            return Err(invalid("same token"));
        }
        Self::quote_on(&self.named_pool(tin, tout)?, tin, tout, amt)
    }

    fn swap(&self, tin: &Bytes, tout: &Bytes, amt: u128) -> R<(Swapped, Self)> {
        if tin == tout {
            return Err(invalid("same token"));
        }
        self.swap_on(&self.named_pool(tin, tout)?, tin, tout, amt)
    }

    /// [`Self::swap`] over a pool already built for `tin`/`tout`: the quote, then the books it
    /// settles. The one path `get_amount_out` and `get_limits` both take.
    fn swap_on(&self, p: &NamedPool, tin: &Bytes, tout: &Bytes, amt: u128) -> R<(Swapped, Self)> {
        let s = Self::quote_on(p, tin, tout, amt)?;
        let mut next = self.clone();
        let book = |n: &mut Self| -> Option<()> {
            n.legs.get_mut(tin)?.settle(amt, 0, 0)?;
            n.legs
                .get_mut(tout)?
                .settle(0, s.out.checked_add(s.proto_fee)?, s.lp_fee)
        };
        book(&mut next).ok_or_else(|| invalid("settled books leave u128"))?;
        Ok((s, next))
    }

    fn amount(x: &BigUint) -> R<u128> {
        x.to_u128()
            .ok_or_else(|| invalid("amount exceeds u128"))
    }
}

#[typetag::serde]
impl ProtocolSim for BtrAimmState {
    /// Lowest `minFee` over the listed non-base legs: the floor of what any swap pays. 0 with no
    /// spoke listed.
    fn fee(&self) -> f64 {
        let base = self.base();
        self.legs
            .iter()
            .filter(|(t, _)| **t != base)
            .map(|(_, l)| l.asset().min_fee_pbps())
            .min()
            .map_or(0.0, |f| f64::from(f) / btr_core::PBPS.l[0] as f64)
    }

    /// `quote` per `base` from a probe swap of a hundredth of a quote unit, so it includes the fee
    /// and the pool's skew but not size impact.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let probe = 10u128
            .pow(quote.decimals.min(30))
            .max(100) /
            100;
        let s = self.quote(&quote.address, &base.address, probe)?;
        let paid = probe as f64 / 10f64.powi(quote.decimals as i32);
        let got = s.out as f64 / 10f64.powi(base.decimals as i32);
        Ok(paid / got)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let (s, next) =
            self.swap(&token_in.address, &token_out.address, Self::amount(&amount_in)?)?;
        Ok(GetAmountOutResult::new(BigUint::from(s.out), BigUint::from(SWAP_GAS), Box::new(next)))
    }

    /// The largest input the pool still fills unclamped (doubling then bisection over the quote,
    /// searched to [`LIMIT_CAP_X`] times the larger of the two reserves), and what it pays for it.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        if sell_token == buy_token {
            return Err(invalid("same token"));
        }
        let pool = self.named_pool(&sell_token, &buy_token)?;
        let ok = |a: u128| {
            self.swap_on(&pool, &sell_token, &buy_token, a)
                .ok()
                .map(|(s, _)| s)
        };
        let cap = self
            .leg(&sell_token)?
            .endpoint()
            .reserves
            .max(
                self.leg(&buy_token)?
                    .endpoint()
                    .reserves,
            )
            .saturating_mul(LIMIT_CAP_X)
            .max(1);
        let lo = max_fill(cap, |a| ok(a).is_some()).ok_or_else(|| invalid("no fillable size"))?;
        let out = ok(lo).map_or(0, |s| s.out);
        Ok((BigUint::from(lo), BigUint::from(out)))
    }

    /// Atomic: the delta lands on a clone that must still validate, or the state is untouched.
    /// It must carry `block_timestamp` (the quote clock; see the decoder docs).
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &std::collections::HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if !delta
            .updated_attributes
            .contains_key("block_timestamp")
        {
            return Err(TransitionError::MissingAttribute("block_timestamp".into()));
        }
        let mut next = self.clone();
        for (k, v) in delta.updated_attributes {
            apply_attribute(&mut next, &k, Some(&v)).map_err(TransitionError::DecodeError)?;
        }
        for k in delta.deleted_attributes {
            apply_attribute(&mut next, &k, None).map_err(TransitionError::DecodeError)?;
        }
        next.validate()
            .map_err(TransitionError::DecodeError)?;
        *self = next;
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tycho_common::models::Chain;

    use super::{super::decoder::state_from_attributes, *};

    /// `bell_100_inv` as `eth_getCode` returned it (btr-core `tests/fixtures`): `build` of
    /// interior [4012, 4997, 7221, 7853], 9 knots, dispersion ref 100.
    const BLOB: &str = "00000064138e0000000000000000000000000000000000001ead1c3513850fac05fffffffff7a546d500000003ed6755ec0000000d5dcceef7ffffffe8b789180000000000000000000000000000000000000000000000000000000000406338600000000003b01367000000005260da54000000056edb68d5fffffffa3ac5dc180000000000000000ffffffffffffffffffff1106a1533e07fffffffff2689b6ffffffffdb8ac136c000000003ca1e6da0000000d56999a6efffffffff21ace170000000000000000ffffffffffffffffffff05a947e5e42100000000dd3600b80000000006eb5211ffffffffe2c2975200000002f6f274080000000c1b3863830000000000000000ffffffffffffffffffff3c5dc12db4a4fffffffffdc44b1cffffffffb10d6f3bffffffff0378f1960000000973c3adae0000000ef99d0c0a0000000000000000ffffffffffffffffffff5dd4e2c164eb00000000268fcd77";
    const BASE: u8 = 1;
    const USDT: u8 = 2;
    const E24: u128 = 1_000_000_000_000_000_000_000_000;

    fn addr(b: u8) -> Bytes {
        Bytes::from(vec![b; 20])
    }

    fn token(b: u8) -> Token {
        Token::new(&addr(b), "T", 18, 0, &[], Chain::Monad, 100)
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn put(w: &mut [u8; 32], offset: usize, v: &[u8]) {
        w[32 - offset - v.len()..32 - offset].copy_from_slice(v);
    }

    /// The `replica.rs` test fixture: R = L = 1e24, minFee 50 pbps, kappa 5000, 18 decimals,
    /// proto share 20, block 1000, obs 1000, ttl 60, conf 5, sigma 100 pbps (floor 64).
    /// Marks are 2^60 (1e18 has no 25-bit float): the P8 lane is `36 << 24 | 9 << 42 | 5 << 50`.
    fn attrs(obs_delta: u64) -> HashMap<String, Bytes> {
        use st::pool_structs::asset as a;
        let b = |w: [u8; 32]| Bytes::from(w.to_vec());
        let mut slot0 = [0u8; 32];
        put(&mut slot0, 0, &[BASE; 20]);
        put(&mut slot0, 21, &[20]);
        let class = 60u64 | 50 << 12 | 64 << 23;
        let (mut lane_cls, mut cls_a) = ([0u8; 32], [0u8; 32]);
        lane_cls[31] = 0b001_001;
        cls_a[24..].copy_from_slice(&class.to_be_bytes());
        let classes = [lane_cls, cls_a, [0u8; 32]].concat();
        let lane = 36u64 << 24 | obs_delta << 30 | 9 << 42 | 5 << 50;
        let mut p = [0u8; 32];
        p[..4].copy_from_slice(&1000u32.to_be_bytes());
        p[16..].copy_from_slice(&(u128::from(lane) | u128::from(lane) << 56).to_be_bytes());
        let mut w0 = [0u8; 32];
        put(&mut w0, a::RESERVES.1, &E24.to_be_bytes());
        put(&mut w0, a::LIABILITIES.1, &E24.to_be_bytes());
        let w2 = |lane: u8| {
            let mut w = [0u8; 32];
            put(&mut w, a::MIN_DISPERSION_PBPS.1, &20_000u32.to_be_bytes());
            put(&mut w, a::CURVE_ID.1, &1u16.to_be_bytes());
            put(&mut w, a::MIN_FEE_PBPS.1, &50u16.to_be_bytes());
            put(&mut w, a::DECIMALS.1, &[18]);
            put(&mut w, a::FLAGS.1, &btr_core::SWAP_ENABLED_BIT.to_be_bytes());
            put(&mut w, a::KAPPA_COV_BPS.1, &5000u16.to_be_bytes());
            put(&mut w, a::ORACLE_BITS.1, &[lane]);
            w
        };
        let mut m: HashMap<String, Bytes> = HashMap::from([
            ("slot0".into(), b(slot0)),
            ("classes".into(), Bytes::from(classes)),
            ("curve/1".into(), Bytes::from(hex_bytes(BLOB))),
        ]);
        for (t, lane) in [(BASE, 1u8), (USDT, 0)] {
            let h = hex::encode([t; 20]);
            for (f, w) in [("w0", w0), ("w2", w2(lane)), ("p", p), ("r", [0u8; 32])] {
                m.insert(format!("leg/{h}/{f}"), b(w));
            }
        }
        m
    }

    fn state() -> BtrAimmState {
        state_from_attributes(&attrs(0), 1000).unwrap()
    }

    /// The exact-value golden lives in `btr_core::replica`
    /// (`quote_hop_is_the_hand_built_pool_...`): this fixture is the same pool, so the state
    /// must return its `amount_out` and book the swap.
    #[test]
    fn quotes_match_the_replica_law_both_ways() {
        let s = state();
        let e21 = 1_000_000_000_000_000_000_000u128;
        for (i, o, want) in
            [(USDT, BASE, 1152242029591071039533u128), (BASE, USDT, 866917742750102823209)]
        {
            let r = s
                .get_amount_out(BigUint::from(e21), &token(i), &token(o))
                .unwrap();
            assert_eq!(r.amount, BigUint::from(want));
            // the out leg paid out + proto fee from reserves, the in leg took the deposit
            let n = r
                .new_state
                .as_any()
                .downcast_ref::<BtrAimmState>()
                .unwrap();
            assert!(
                n.leg(&addr(o))
                    .unwrap()
                    .endpoint()
                    .reserves <
                    E24
            );
            assert_eq!(
                n.leg(&addr(i))
                    .unwrap()
                    .endpoint()
                    .reserves,
                E24 + e21
            );
        }
    }

    #[test]
    fn leg_settle_refuses_what_the_pool_reverts_on_and_keeps_the_word() {
        let mut leg = *state().leg(&addr(USDT)).unwrap();
        let mut w0 = leg.w0.0;
        put(&mut w0, 0, &(u128::MAX - 10).to_be_bytes());
        leg.w0 = B256::from(w0);
        let before = leg;
        assert_eq!(leg.settle(11, 0, 0), None);
        assert_eq!(leg.settle(0, u128::MAX, 0), None);
        assert_eq!(leg, before);
        assert_eq!(leg.settle(10, 0, 0), Some(()));
        assert_eq!(leg.endpoint().reserves, u128::MAX);
    }

    #[test]
    fn the_gate_refuses_what_the_pool_reverts_on() {
        let e21 = BigUint::from(10u128.pow(21));
        let mut s = state();
        // ttl 60, grace 30: 60 s old still quotes, 61 s does not
        s.ts = 1060;
        assert!(s
            .get_amount_out(e21.clone(), &token(USDT), &token(BASE))
            .is_ok());
        s.ts = 1061;
        assert!(s
            .get_amount_out(e21.clone(), &token(USDT), &token(BASE))
            .is_err());
        // a halted leg drops out
        let mut s = state();
        s.legs
            .get_mut(&addr(USDT))
            .unwrap()
            .w2
            .0[32 - 23 - 2] = 0;
        s.legs
            .get_mut(&addr(USDT))
            .unwrap()
            .w2
            .0[32 - 23 - 1] = 1;
        assert!(s
            .get_amount_out(e21.clone(), &token(USDT), &token(BASE))
            .is_err());
        // an amount past u128 is refused, not a panic
        let huge = BigUint::from(u128::MAX) + 1u32;
        assert!(state()
            .get_amount_out(huge, &token(USDT), &token(BASE))
            .is_err());
    }

    #[test]
    fn limits_bind() {
        let s = state();
        let (max_in, max_out) = s
            .get_limits(addr(USDT), addr(BASE))
            .unwrap();
        assert!(max_in > BigUint::ZERO && max_out > BigUint::ZERO);
        let ok = |a: &BigUint| {
            s.get_amount_out(a.clone(), &token(USDT), &token(BASE))
                .is_ok()
        };
        assert!(ok(&max_in));
        assert!(!ok(&(&max_in + 1u32)));
        assert!(s
            .get_limits(addr(USDT), addr(USDT))
            .is_err());
    }

    #[test]
    fn max_fill_searches_to_the_cap() {
        // the wall binds inside the cap: exact, whatever the cap's shape
        for wall in [1u128, 2, 3, 1000, 1023, 1024, 65_537] {
            for cap in [wall, wall + 1, 1 << 20, 3 * 1_000_003] {
                let want = wall.min(cap);
                assert_eq!(max_fill(cap, |a| a <= wall), Some(want), "wall {wall} cap {cap}");
            }
        }
        // no wall: the cap itself is tested and returned, even when it is no power of two
        assert_eq!(max_fill(1_000_003, |_| true), Some(1_000_003));
        assert_eq!(max_fill(1, |_| true), Some(1));
        // nothing fills; tiny sizes that fail before the first success do not end the search
        assert_eq!(max_fill(1 << 30, |_| false), None);
        assert_eq!(max_fill(1000, |a| (5..=300).contains(&a)), Some(300));
        // a cap saturated to u128::MAX (reserves * 16 overflowed): no overflow in the doubling
        // or the bisection, whether the wall sits low, past 2^127, or nowhere
        assert_eq!(max_fill(u128::MAX, |a| a <= 1000), Some(1000));
        assert_eq!(max_fill(u128::MAX, |a| a <= u128::MAX - 5), Some(u128::MAX - 5));
        assert_eq!(max_fill(u128::MAX, |a| a <= 1 << 127), Some(1 << 127));
        assert_eq!(max_fill(u128::MAX, |_| true), Some(u128::MAX));
        assert_eq!(max_fill(u128::MAX, |_| false), None);
    }

    fn delta(updated: &[(String, Bytes)], deleted: &[&str], ts: Option<u64>) -> ProtocolStateDelta {
        let mut u: HashMap<String, Bytes> = updated.iter().cloned().collect();
        if let Some(t) = ts {
            u.insert("block_timestamp".into(), Bytes::from(t.to_be_bytes().to_vec()));
        }
        ProtocolStateDelta {
            updated_attributes: u,
            deleted_attributes: deleted
                .iter()
                .map(|k| k.to_string())
                .collect(),
            ..Default::default()
        }
    }

    fn apply(s: &mut BtrAimmState, d: ProtocolStateDelta) -> Result<(), TransitionError> {
        s.delta_transition(d, &HashMap::new(), &Balances::default())
    }

    #[test]
    fn deltas_land_atomically_and_carry_the_clock() {
        let mut s = state();
        let before = s.clone();
        let hb = hex::encode([BASE; 20]);
        let mut w0 = s.leg(&addr(BASE)).unwrap().w0.0;
        put(&mut w0, 0, &(E24 / 2).to_be_bytes());
        let w0 = (format!("leg/{hb}/w0"), Bytes::from(w0.to_vec()));
        // a fresh reserves word and clock land
        apply(&mut s, delta(std::slice::from_ref(&w0), &[], Some(1030))).unwrap();
        assert_eq!(s.ts, 1030);
        assert_eq!(
            s.leg(&addr(BASE))
                .unwrap()
                .endpoint()
                .reserves,
            E24 / 2
        );
        // no clock: refused whole, state untouched
        let mut s = before.clone();
        let e = apply(&mut s, delta(std::slice::from_ref(&w0), &[], None));
        assert!(matches!(e, Err(TransitionError::MissingAttribute(_))));
        assert_eq!(s, before);
        // one bad key voids the good ones beside it
        let bad = ("bogus".to_string(), Bytes::default());
        assert!(apply(&mut s, delta(&[w0.clone(), bad], &[], Some(1030))).is_err());
        assert_eq!(s, before);
        // a delta that leaves an undecodable pool (last curve gone) is refused, not applied
        assert!(apply(&mut s, delta(&[], &["curve/1"], Some(1030))).is_err());
        assert_eq!(s, before);
    }

    #[test]
    fn deletes_zero_a_leg_word_and_refuse_structural_keys() {
        let mut s = state();
        let hu = hex::encode([USDT; 20]);
        // the tier word cleared on chain: the leg has no mark, the quote errors
        apply(&mut s, delta(&[], &[&format!("leg/{hu}/p")], Some(1000))).unwrap();
        assert_eq!(s.leg(&addr(USDT)).unwrap().p, B256::ZERO);
        assert!(s
            .get_amount_out(BigUint::from(10u128.pow(21)), &token(USDT), &token(BASE))
            .is_err());
        // all four words cleared: the leg drops out; deleting an unlisted leg's word is a no-op
        let all: Vec<String> = ["w0", "w2", "p", "r"]
            .iter()
            .map(|f| format!("leg/{hu}/{f}"))
            .collect();
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        apply(&mut s, delta(&[], &all, Some(1000))).unwrap();
        assert!(!s.legs.contains_key(&addr(USDT)));
        apply(&mut s, delta(&[], &all, Some(1000))).unwrap();
        // only structural keys reject
        for k in ["slot0", "classes", "block_timestamp", "leg/zz/w0", "leg/nofield"] {
            assert!(apply(&mut state(), delta(&[], &[k], Some(1000))).is_err(), "{k}");
        }
        assert!(apply(&mut state(), delta(&[], &["curve/2"], Some(1000))).is_ok());
        assert!(apply(&mut state(), delta(&[], &[&format!("leg/{hu}/w1")], Some(1000))).is_err());
    }

    #[test]
    fn a_delta_that_zeroes_the_base_leg_is_refused_whole() {
        let mut s = state();
        let before = s.clone();
        let hb = hex::encode([BASE; 20]);
        let all: Vec<String> = ["w0", "w2", "p", "r"]
            .iter()
            .map(|f| format!("leg/{hb}/{f}"))
            .collect();
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        // the base leg would drop out of the map: refused, and the good delete beside it too
        let e = apply(&mut s, delta(&[], &all, Some(1030)));
        assert!(matches!(e, Err(TransitionError::DecodeError(_))), "{e:?}");
        assert_eq!(s, before);
        // the base's reserves word alone zeroing is still a listed leg
        apply(&mut s, delta(&[], &[&format!("leg/{hb}/w0")], Some(1030))).unwrap();
        assert!(s.legs.contains_key(&addr(BASE)));
    }

    #[test]
    fn a_swap_gated_leg_is_refused() {
        let mut s = state();
        let l = s.legs.get_mut(&addr(USDT)).unwrap();
        let f = 32 - 23 - 2;
        let flags = u16::from_be_bytes([l.w2.0[f], l.w2.0[f + 1]]) | btr_core::SWAP_GATED_BIT;
        l.w2.0[f..f + 2].copy_from_slice(&flags.to_be_bytes());
        assert!(s
            .get_amount_out(BigUint::from(10u128.pow(21)), &token(USDT), &token(BASE))
            .is_err());
        assert!(s
            .get_limits(addr(USDT), addr(BASE))
            .is_err());
    }

    const E21: u128 = 10u128.pow(21);

    fn set_flags(s: &mut BtrAimmState, t: u8, flags: u16) {
        let w2 = &mut s.legs.get_mut(&addr(t)).unwrap().w2.0;
        put(w2, st::pool_structs::asset::FLAGS.1, &flags.to_be_bytes());
    }

    fn set_book(s: &mut BtrAimmState, t: u8, reserves: u128, liabilities: u128) {
        use st::pool_structs::asset as a;
        let w0 = &mut s.legs.get_mut(&addr(t)).unwrap().w0.0;
        put(w0, a::RESERVES.1, &reserves.to_be_bytes());
        put(w0, a::LIABILITIES.1, &liabilities.to_be_bytes());
    }

    fn out_of(s: &BtrAimmState, i: u8, o: u8) -> R<u128> {
        let r = s.get_amount_out(BigUint::from(E21), &token(i), &token(o))?;
        Ok(r.amount.to_u128().unwrap())
    }

    #[test]
    fn settle_overflow_is_invalid_input_through_get_amount_out() {
        // the deposit would push the in leg's reserves past u128; the quote itself is fine
        let mut s = state();
        set_book(&mut s, USDT, u128::MAX - E21 / 2, E24);
        let e = s.get_amount_out(BigUint::from(E21), &token(USDT), &token(BASE));
        assert!(
            matches!(&e, Err(SimulationError::InvalidInput(m, _)) if m.contains("settled books")),
            "{:?}",
            e.err()
        );
        // the same books one unit inside the edge settle
        set_book(&mut s, USDT, u128::MAX - E21, E24);
        assert!(s
            .get_amount_out(BigUint::from(E21), &token(USDT), &token(BASE))
            .is_ok());
    }

    #[test]
    fn limits_stop_where_settlement_would_overflow() {
        let mut s = state();
        set_book(&mut s, USDT, u128::MAX - 10 * E21, E24);
        let (max_in, _) = s
            .get_limits(addr(USDT), addr(BASE))
            .unwrap();
        let ok = |a: &BigUint| {
            s.get_amount_out(a.clone(), &token(USDT), &token(BASE))
                .is_ok()
        };
        assert!(ok(&max_in) && !ok(&(&max_in + 1u32)));
        assert!(max_in <= BigUint::from(10 * E21));
    }

    #[test]
    fn a_spoke_to_spoke_quote_prices_two_hops_and_books_both_legs() {
        const OTHER: u8 = 3;
        let mut s = state();
        s.legs
            .insert(addr(OTHER), *s.leg(&addr(USDT)).unwrap());
        let r = s
            .get_amount_out(BigUint::from(E21), &token(USDT), &token(OTHER))
            .unwrap();
        let out = r.amount.to_u128().unwrap();
        // equal marks: par less the two hops' fees
        assert!(out < E21 && out > E21 * 95 / 100, "{out}");
        let n = r
            .new_state
            .as_any()
            .downcast_ref::<BtrAimmState>()
            .unwrap();
        assert_eq!(
            n.leg(&addr(USDT))
                .unwrap()
                .endpoint()
                .reserves,
            E24 + E21
        );
        assert!(
            n.leg(&addr(OTHER))
                .unwrap()
                .endpoint()
                .reserves <
                E24
        );
        assert_eq!(n.leg(&addr(BASE)).unwrap(), s.leg(&addr(BASE)).unwrap());
        // the reverse direction quotes too
        assert!(out_of(&s, OTHER, USDT).is_ok());
    }

    #[test]
    fn a_uoa_leg_is_rebased_onto_the_base_mark() {
        let plain = out_of(&state(), USDT, BASE).unwrap();
        let mut s = state();
        let w2 = &mut s
            .legs
            .get_mut(&addr(USDT))
            .unwrap()
            .w2
            .0;
        w2[32 - st::pool_structs::asset::ORACLE_BITS.1 - 1] |= 128; // uoa
                                                                    // both marks are 2^60, so the rebased USDT mark is exactly 1.0 base: par less the fee,
                                                                    // where the plain leg pays its 2^60 / 1e18 (~1.153) mark
        let rebased = out_of(&s, USDT, BASE).unwrap();
        assert!(rebased < E21 && rebased > E21 * 99 / 100, "{rebased}");
        assert!(plain > rebased * 11 / 10, "{plain} {rebased}");
        // a dead base mark leaves nothing to rebase onto
        s.legs.get_mut(&addr(BASE)).unwrap().p = B256::ZERO;
        assert!(out_of(&s, USDT, BASE).is_err());
    }

    #[test]
    fn a_delta_changes_the_quote() {
        let mut s = state();
        let before = out_of(&s, USDT, BASE).unwrap();
        let hu = hex::encode([USDT; 20]);
        let mut w0 = s.leg(&addr(USDT)).unwrap().w0.0;
        put(&mut w0, 16, &(E24 / 4).to_be_bytes());
        let d = delta(&[(format!("leg/{hu}/w0"), Bytes::from(w0.to_vec()))], &[], Some(1000));
        apply(&mut s, d).unwrap();
        assert_ne!(out_of(&s, USDT, BASE).unwrap(), before);
    }

    #[test]
    fn the_mode_picks_the_gate() {
        let on = btr_core::SWAP_ENABLED_BIT;
        let coop = btr_core::COOP_ENABLED_BIT;
        let gate = btr_core::SWAP_GATED_BIT;
        let quotes = |s: &BtrAimmState| out_of(s, USDT, BASE).is_ok();
        let with = |mode, usdt, base| {
            let mut s = state();
            s.mode = mode;
            set_flags(&mut s, USDT, usdt);
            set_flags(&mut s, BASE, base);
            s
        };
        // public: SWAP_GATED closes a leg, COOP_ENABLED is irrelevant
        assert!(quotes(&with(Mode::Public, on, on)));
        assert!(quotes(&with(Mode::Public, on | coop, on | coop)));
        assert!(!quotes(&with(Mode::Public, on | gate | coop, on)));
        // coop: both legs need COOP_ENABLED, SWAP_GATED is ignored
        assert!(!quotes(&with(Mode::Coop, on, on)));
        assert!(!quotes(&with(Mode::Coop, on | coop, on)));
        assert!(!quotes(&with(Mode::Coop, on, on | coop)));
        assert!(quotes(&with(Mode::Coop, on | coop, on | coop)));
        assert!(quotes(&with(Mode::Coop, on | coop | gate, on | coop)));
        // a halt bit closes both
        assert!(!quotes(&with(Mode::Coop, on | coop | 1, on | coop)));
        // limits follow the same gate
        let closed = with(Mode::Coop, on, on);
        assert!(closed
            .get_limits(addr(USDT), addr(BASE))
            .is_err());
    }

    #[test]
    fn the_mode_attribute_selects_the_gate() {
        let mode = |v: &[u8]| {
            let mut a = attrs(0);
            a.insert("mode".into(), Bytes::from(v.to_vec()));
            state_from_attributes(&a, 1000)
        };
        assert_eq!(state().mode, Mode::Public);
        assert_eq!(mode(b"public").unwrap().mode, Mode::Public);
        assert_eq!(mode(b"coop").unwrap().mode, Mode::Coop);
        assert!(mode(b"Coop").is_err());
        assert!(apply(&mut state(), delta(&[], &["mode"], Some(1000))).is_err());
    }

    #[test]
    fn spot_price_and_fee_read_the_pool() {
        let s = state();
        let px = s
            .spot_price(&token(BASE), &token(USDT))
            .unwrap();
        // a base costs 1e18 / 2^60 of the spoke, plus the fee
        let par = 1e18 / 2f64.powi(60);
        assert!(px > par && px < par * 1.005, "{px}");
        assert_eq!(s.fee(), 50.0 / 1e6);
        // no spoke listed: no fee floor, not f64::MAX
        let mut bare = state();
        bare.legs
            .retain(|t, _| *t == addr(BASE));
        assert_eq!(bare.fee(), 0.0);
    }

    #[test]
    fn a_snapshot_missing_its_words_is_refused() {
        for k in ["slot0", "classes", "curve/1"] {
            let mut a = attrs(0);
            a.remove(k);
            assert!(state_from_attributes(&a, 1000).is_err(), "{k}");
        }
        let mut a = attrs(0);
        a.insert("bogus".into(), Bytes::default());
        assert!(state_from_attributes(&a, 1000).is_err());
    }
}
