use std::str::FromStr;

use crate::pb::uniswap::v4::{
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

pub fn event_to_ticks_deltas(event: PoolEvent) -> Vec<TickDelta> {
    // On UniswapV4, the only event that changes liquidity is ModifyLiquidity. Liquidity Delta is
    // now expressed as a signed int256. A positive number indicates a mint, while a negative
    // indicates a burn.
    // Mint events will have negative deltas for the upper tick and positive deltas for the lower.
    // Burn events will have positive deltas for the upper tick and negative deltas for the lower.
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::ModifyLiquidity(liq_change) => {
            let amount =
                BigInt::from_str(&liq_change.liquidity_delta).expect("Failed to parse BigInt");
            vec![
                TickDelta {
                    pool_address: hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
                    tick_index: liq_change.tick_lower,
                    liquidity_net_delta: amount.to_signed_bytes_be(),
                    ordinal: event.log_ordinal,
                    transaction: event.transaction.clone(),
                },
                TickDelta {
                    pool_address: hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
                    tick_index: liq_change.tick_upper,
                    liquidity_net_delta: amount.neg().to_signed_bytes_be(),
                    ordinal: event.log_ordinal,
                    transaction: event.transaction,
                },
            ]
        }
        _ => vec![],
    }
}
