use std::str::FromStr;

use crate::pb::uniswap::v3::{
    events::{pool_event, PoolEvent},
    Events, TickDelta, TickDeltas,
};

use substreams::scalar::BigInt;

use anyhow::Ok;

#[substreams::handlers::map]
pub fn map_ticks_changes(events: Events) -> Result<TickDeltas, anyhow::Error> {
    let ticks_deltas = events
        .pool_events
        .into_iter()
        .flat_map(event_to_ticks_deltas)
        .collect();

    Ok(TickDeltas { deltas: ticks_deltas })
}

pub(crate) fn event_to_ticks_deltas(event: PoolEvent) -> Vec<TickDelta> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Mint(mint) => {
            vec![
                TickDelta {
                    pool_address: hex::decode(&event.pool_address).unwrap(),
                    tick_index: mint.tick_lower,
                    liquidity_net_delta: BigInt::from_str(&mint.amount)
                        .unwrap()
                        .to_signed_bytes_be(),
                    ordinal: event.log_ordinal,
                    transaction: event.transaction.clone(),
                },
                TickDelta {
                    pool_address: hex::decode(&event.pool_address).unwrap(),
                    tick_index: mint.tick_upper,
                    liquidity_net_delta: BigInt::from_str(&mint.amount)
                        .unwrap()
                        .neg()
                        .to_signed_bytes_be(),
                    ordinal: event.log_ordinal,
                    transaction: event.transaction,
                },
            ]
        }
        pool_event::Type::Burn(burn) => vec![
            TickDelta {
                pool_address: hex::decode(&event.pool_address).unwrap(),
                tick_index: burn.tick_lower,
                liquidity_net_delta: BigInt::from_str(&burn.amount)
                    .unwrap()
                    .neg()
                    .to_signed_bytes_be(),
                ordinal: event.log_ordinal,
                transaction: event.transaction.clone(),
            },
            TickDelta {
                pool_address: hex::decode(&event.pool_address).unwrap(),
                tick_index: burn.tick_upper,
                liquidity_net_delta: BigInt::from_str(&burn.amount)
                    .unwrap()
                    .to_signed_bytes_be(),
                ordinal: event.log_ordinal,
                transaction: event.transaction,
            },
        ],
        _ => vec![],
    }
}
