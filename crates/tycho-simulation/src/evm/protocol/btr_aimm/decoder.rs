//! Tycho attributes -> `BtrAimmState`. Every value is a raw storage word (big-endian, left-padded
//! to 32 bytes) unless noted.
//!
//! * `slot0`: pool storage slot 0 (base token, protocol share).
//! * `classes`: the impl's `MarkStoreP8.classes()` words `laneCls | clsA | clsB`, 96 bytes.
//! * `block_timestamp`: block time, big-endian integer; the quote's staleness clock. Absent from a
//!   snapshot = the snapshot header's timestamp. Every delta MUST carry it (the substream writes it
//!   each block the pool changes): a delta without it is refused, since a clock that does not
//!   advance judges old marks fresh.
//! * `leg/<token>/w0|w2`: the `Asset` words 0 (reserves | liabilities) and 2 (risk + oracle wiring)
//!   of `assets[token]`; `<token>` = 40 lowercase hex chars, no `0x`. The base is a leg too.
//! * `leg/<token>/p|r`: the P and R tier words of the token's oracle lane in the impl's
//!   `MarkStoreP8` (`p8::tier_slot(1|2, lane)`).
//! * `mode`: `coop` or `public` (ASCII), the swap entry the legs are gated for (see `Mode`). A
//!   static attribute of the component; absent = `public`.
//! * `curve/<id>`: the curve blob, raw `eth_getCode` at `curve_pointer(pool, id)`. Deletable.
//!
//! Deletes: a `leg/*` word deleted reads as zero (the slot was cleared; a leg with all four words
//! zero drops out) and a `curve/<id>` deleted removes the blob. Any other key is structural and a
//! delete of it is refused.
use std::collections::HashMap;

use alloy::primitives::B256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::{BtrAimmState, Leg, Mode};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

fn word(v: &Bytes) -> Result<B256, String> {
    if v.len() > 32 {
        return Err(format!("word longer than 32 bytes: {}", v.len()));
    }
    let mut w = [0u8; 32];
    w[32 - v.len()..].copy_from_slice(v.as_ref());
    Ok(B256::from(w))
}

fn ts(v: &Bytes) -> Result<u64, String> {
    if v.len() > 8 {
        return Err(format!("timestamp longer than 8 bytes: {}", v.len()));
    }
    Ok(v.iter()
        .fold(0u64, |a, b| a << 8 | u64::from(*b)))
}

/// Applies one attribute; `None` = deleted (see the module docs for what a delete means).
pub(super) fn apply_attribute(
    s: &mut BtrAimmState,
    key: &str,
    v: Option<&Bytes>,
) -> Result<(), String> {
    if let Some(id) = key.strip_prefix("curve/") {
        let id: u16 = id
            .parse()
            .map_err(|_| format!("bad curve key {key}"))?;
        match v {
            Some(b) => s.curves.insert(id, b.clone()),
            None => s.curves.remove(&id),
        };
        return Ok(());
    }
    if let Some(rest) = key.strip_prefix("leg/") {
        let (token, field) = rest
            .split_once('/')
            .ok_or_else(|| format!("bad leg key {key}"))?;
        let token = hex::decode(token).map_err(|_| format!("bad leg token in {key}"))?;
        if token.len() != 20 {
            return Err(format!("bad leg token in {key}"));
        }
        let token = Bytes::from(token);
        if !matches!(field, "w0" | "w2" | "p" | "r") {
            return Err(format!("unknown attribute {key}"));
        }
        let w = v
            .map(word)
            .transpose()?
            .unwrap_or_default();
        if v.is_none() && !s.legs.contains_key(&token) {
            return Ok(());
        }
        let leg = s.legs.entry(token.clone()).or_default();
        match field {
            "w0" => leg.w0 = w,
            "w2" => leg.w2 = w,
            "p" => leg.p = w,
            _ => leg.r = w,
        }
        if *leg == Leg::default() {
            s.legs.remove(&token);
        }
        return Ok(());
    }
    let v = v.ok_or_else(|| format!("structural attribute {key} deleted"))?;
    match key {
        "slot0" => s.slot0 = word(v)?,
        "classes" => {
            if v.len() != 96 {
                return Err(format!("classes must be 96 bytes, got {}", v.len()));
            }
            for (c, w) in s.classes.iter_mut().zip(v.chunks(32)) {
                *c = B256::from_slice(w);
            }
        }
        "block_timestamp" => s.ts = ts(v)?,
        "mode" => {
            s.mode = match v.as_ref() {
                b"coop" => Mode::Coop,
                b"public" => Mode::Public,
                _ => return Err("mode must be `coop` or `public`".into()),
            }
        }
        // Tycho-injected, not ours.
        "block_number" => {}
        _ => return Err(format!("unknown attribute {key}")),
    }
    Ok(())
}

pub fn state_from_attributes(
    attrs: &HashMap<String, Bytes>,
    header_ts: u64,
) -> Result<BtrAimmState, String> {
    let mut s = BtrAimmState { ts: header_ts, ..Default::default() };
    for (k, v) in attrs {
        apply_attribute(&mut s, k, Some(v))?;
    }
    s.validate()?;
    Ok(s)
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for BtrAimmState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let mut attrs = snapshot
            .component
            .static_attributes
            .clone();
        attrs.extend(snapshot.state.attributes);
        state_from_attributes(&attrs, block.timestamp).map_err(InvalidSnapshotError::ValueError)
    }
}
