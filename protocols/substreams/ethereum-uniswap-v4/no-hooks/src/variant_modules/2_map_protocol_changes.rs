use substreams_ethereum::pb::eth::v2::{self as eth};
use tycho_substreams::prelude::*;

use ethereum_uniswap_v4_shared::utils::protocol_changes::collect_transaction_changes;

use crate::pb::uniswap::v4::{Events, LiquidityChanges, TickDeltas};

#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::Block,
    created_pools: BlockEntityChanges,
    events: Events,
    balance_deltas: BlockBalanceDeltas,
    tick_deltas: TickDeltas,
    liquidity_changes: LiquidityChanges,
) -> Result<BlockChanges, substreams::errors::Error> {
    let changes = collect_transaction_changes(
        created_pools,
        events,
        balance_deltas,
        tick_deltas,
        liquidity_changes,
    );

    Ok(BlockChanges { block: Some((&block).into()), changes, storage_changes: vec![] })
}
