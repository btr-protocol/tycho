use std::str::FromStr;

use substreams::store::{StoreGet, StoreGetInt64};

use crate::pb::uniswap::v4::{
    events::{pool_event, PoolEvent},
    Events, LiquidityChange, LiquidityChangeType, LiquidityChanges,
};

use substreams::scalar::BigInt;

#[substreams::handlers::map]
pub fn map_liquidity_changes(
    events: Events,
    pools_current_tick_store: StoreGetInt64,
) -> Result<LiquidityChanges, anyhow::Error> {
    liquidity_changes(events, |event| {
        pools_current_tick_store.get_at(event.log_ordinal, format!("pool:{0}", event.pool_id))
    })
}

/// Returns the liquidity changes of `events`, ordered by ordinal. `current_tick` gives a pool's
/// tick before an event.
///
/// Errors on a liquidity change of a pool whose tick is unknown: `Initialize` precedes a pool's
/// first liquidity change and a snapshot seeds the tick of every older pool, so this means the
/// stream started after the pool's creation without a snapshot.
pub fn liquidity_changes(
    events: Events,
    current_tick: impl Fn(&PoolEvent) -> Option<i64>,
) -> Result<LiquidityChanges, anyhow::Error> {
    let mut changes = Vec::new();
    for event in events
        .pool_events
        .into_iter()
        .filter(PoolEvent::can_introduce_liquidity_changes)
    {
        let tick = current_tick(&event);
        if let Some(change) = event_to_liquidity_deltas(tick, event)? {
            changes.push(change);
        }
    }
    changes.sort_unstable_by_key(|l| l.ordinal);
    Ok(LiquidityChanges { changes })
}

fn event_to_liquidity_deltas(
    current_tick: Option<i64>,
    event: PoolEvent,
) -> Result<Option<LiquidityChange>, anyhow::Error> {
    Ok(match event.r#type.as_ref().unwrap() {
        pool_event::Type::ModifyLiquidity(mod_liquidity) => {
            let tick = current_tick.ok_or_else(|| {
                anyhow::anyhow!(
                    "pool {} changed its liquidity before its tick was known",
                    event.pool_id
                )
            })?;
            if tick >= mod_liquidity.tick_lower.into() && tick < mod_liquidity.tick_upper.into() {
                Some(LiquidityChange {
                    pool_address: hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
                    value: BigInt::from_str(&mod_liquidity.liquidity_delta)
                        .unwrap()
                        .to_signed_bytes_be(),
                    change_type: LiquidityChangeType::Delta.into(),
                    ordinal: event.log_ordinal,
                    transaction: Some(event.transaction.unwrap()),
                })
            } else {
                None
            }
        }
        pool_event::Type::Swap(swap) => Some(LiquidityChange {
            pool_address: hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
            value: BigInt::from_str(&swap.liquidity)
                .unwrap()
                .to_signed_bytes_be(),
            change_type: LiquidityChangeType::Absolute.into(),
            ordinal: event.log_ordinal,
            transaction: Some(event.transaction.unwrap()),
        }),
        _ => None,
    })
}

impl PoolEvent {
    fn can_introduce_liquidity_changes(&self) -> bool {
        matches!(
            self.r#type.as_ref().unwrap(),
            pool_event::Type::ModifyLiquidity(_) | pool_event::Type::Swap(_)
        )
    }
}
