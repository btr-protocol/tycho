use substreams::store::{StoreNew, StoreSet, StoreSetInt64};

use crate::pb::uniswap::v4::{
    events::{pool_event, PoolEvent},
    Events, SnapshotPools,
};

#[substreams::handlers::store]
pub fn store_pool_current_tick(snapshot: SnapshotPools, events: Events, store: StoreSetInt64) {
    for pool in snapshot.pools {
        store.set(0, pool.store_key(), &i64::from(pool.tick));
    }
    events
        .pool_events
        .into_iter()
        .filter_map(event_to_current_tick)
        .for_each(|(pool, ordinal, new_tick_index)| {
            store.set(ordinal, format!("pool:{pool}"), &new_tick_index.into())
        });
}

pub fn event_to_current_tick(event: PoolEvent) -> Option<(String, u64, i32)> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Initialize(initialize) => {
            Some((event.pool_id, event.log_ordinal, initialize.tick))
        }
        pool_event::Type::Swap(swap) => Some((event.pool_id, event.log_ordinal, swap.tick)),
        _ => None,
    }
}
