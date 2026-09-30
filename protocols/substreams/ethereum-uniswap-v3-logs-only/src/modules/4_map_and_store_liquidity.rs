use std::str::FromStr;

use substreams::store::{StoreGet, StoreGetInt64, StoreSet, StoreSetInt64};

use crate::pb::uniswap::v3::{
    events::{pool_event, PoolEvent},
    Events, LiquidityChange, LiquidityChangeType, LiquidityChanges, SnapshotPools,
};

use substreams::{scalar::BigInt, store::StoreNew};

use anyhow::Ok;

#[substreams::handlers::store]
pub fn store_pool_current_tick(snapshot: SnapshotPools, events: Events, store: StoreSetInt64) {
    for pool in snapshot.pools {
        let address = hex::encode(
            pool.pool
                .expect("map_snapshot emits every pool with its address")
                .address,
        );
        store.set(0, format!("pool:{address}"), &i64::from(pool.tick));
    }
    events
        .pool_events
        .into_iter()
        .filter_map(event_to_current_tick)
        .for_each(|(pool, ordinal, new_tick_index)| {
            store.set(ordinal, format!("pool:{pool}"), &new_tick_index.into())
        });
}

#[substreams::handlers::map]
pub fn map_liquidity_changes(
    events: Events,
    pools_current_tick_store: StoreGetInt64,
) -> Result<LiquidityChanges, anyhow::Error> {
    liquidity_changes(events, |event| {
        pools_current_tick_store.get_at(event.log_ordinal, format!("pool:{0}", event.pool_address))
    })
}

/// Returns the liquidity changes of `events`, ordered by ordinal. `current_tick` gives a pool's
/// tick before an event.
///
/// Errors on a mint or burn of a pool whose tick is unknown. A pool's `Initialize` precedes its
/// first mint, and a snapshot seeds the tick of every older pool, so this means the stream started
/// after the pool's creation without a snapshot.
pub(crate) fn liquidity_changes(
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

fn in_range(
    current_tick: Option<i64>,
    pool: &str,
    tick_lower: i32,
    tick_upper: i32,
) -> Result<bool, anyhow::Error> {
    let tick = current_tick.ok_or_else(|| {
        anyhow::anyhow!("pool {pool} changed its liquidity before its tick was known")
    })?;
    Ok(tick >= tick_lower.into() && tick < tick_upper.into())
}

fn event_to_liquidity_deltas(
    current_tick: Option<i64>,
    event: PoolEvent,
) -> Result<Option<LiquidityChange>, anyhow::Error> {
    Ok(match event.r#type.as_ref().unwrap() {
        pool_event::Type::Mint(mint) => {
            if in_range(current_tick, &event.pool_address, mint.tick_lower, mint.tick_upper)? {
                Some(LiquidityChange {
                    pool_address: hex::decode(event.pool_address).unwrap(),
                    value: BigInt::from_str(&mint.amount)
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
        pool_event::Type::Burn(burn) => {
            if in_range(current_tick, &event.pool_address, burn.tick_lower, burn.tick_upper)? {
                Some(LiquidityChange {
                    pool_address: hex::decode(event.pool_address).unwrap(),
                    value: BigInt::from_str(&burn.amount)
                        .unwrap()
                        .neg()
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
            pool_address: hex::decode(event.pool_address).unwrap(),
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
            pool_event::Type::Mint(_) | pool_event::Type::Burn(_) | pool_event::Type::Swap(_)
        )
    }
}

pub(crate) fn event_to_current_tick(event: PoolEvent) -> Option<(String, u64, i32)> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Initialize(initialize) => {
            Some((event.pool_address, event.log_ordinal, initialize.tick))
        }
        pool_event::Type::Swap(swap) => Some((event.pool_address, event.log_ordinal, swap.tick)),
        _ => None,
    }
}
