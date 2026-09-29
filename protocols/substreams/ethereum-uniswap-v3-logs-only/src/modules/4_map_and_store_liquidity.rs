use std::str::FromStr;

use substreams::store::{StoreGet, StoreGetInt64, StoreSet, StoreSetInt64};

use crate::pb::uniswap::v3::{
    events::{pool_event, PoolEvent},
    Events, LiquidityChange, LiquidityChangeType, LiquidityChanges,
};

use substreams::{scalar::BigInt, store::StoreNew};

use anyhow::Ok;

#[substreams::handlers::store]
pub fn store_pool_current_tick(events: Events, store: StoreSetInt64) {
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
    Ok(liquidity_changes(events, |event| {
        pools_current_tick_store.get_at(event.log_ordinal, format!("pool:{0}", event.pool_address))
    }))
}

/// Returns the liquidity changes of `events`, ordered by ordinal. `current_tick` gives a pool's
/// tick before an event, or `None` when no `Initialize` or `Swap` of the pool has been seen since
/// the stream started.
pub(crate) fn liquidity_changes(
    events: Events,
    current_tick: impl Fn(&PoolEvent) -> Option<i64>,
) -> LiquidityChanges {
    let mut changes = events
        .pool_events
        .into_iter()
        .filter(PoolEvent::can_introduce_liquidity_changes)
        .filter_map(|event| event_to_liquidity_deltas(current_tick(&event), event))
        .collect::<Vec<_>>();

    changes.sort_unstable_by_key(|l| l.ordinal);
    LiquidityChanges { changes }
}

/// Whether a position over `[tick_lower, tick_upper)` is active. Without a known tick, which
/// happens when the stream starts after the pool's creation, the event's token amounts decide: a
/// position in range moves both tokens. This misreads a position whose lower tick equals the
/// current price exactly, and a burn so small that one amount rounds down to zero.
fn in_range(
    current_tick: Option<i64>,
    tick_lower: i32,
    tick_upper: i32,
    amount_0: &str,
    amount_1: &str,
) -> bool {
    match current_tick {
        Some(tick) => tick >= tick_lower.into() && tick < tick_upper.into(),
        None => amount_0 != "0" && amount_1 != "0",
    }
}

fn event_to_liquidity_deltas(
    current_tick: Option<i64>,
    event: PoolEvent,
) -> Option<LiquidityChange> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Mint(mint) => {
            if in_range(
                current_tick,
                mint.tick_lower,
                mint.tick_upper,
                &mint.amount_0,
                &mint.amount_1,
            ) {
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
            if in_range(
                current_tick,
                burn.tick_lower,
                burn.tick_upper,
                &burn.amount_0,
                &burn.amount_1,
            ) {
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
    }
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
