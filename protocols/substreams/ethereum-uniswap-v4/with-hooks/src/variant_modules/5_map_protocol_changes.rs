use ethereum_uniswap_v4_shared::{
    pb::uniswap::v4::{Events, LiquidityChanges, TickDeltas},
    utils::protocol_changes::collect_transaction_changes,
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use tycho_substreams::{block_storage::get_block_storage_changes, prelude::*};

#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::Block,
    created_pools: BlockEntityChanges,
    events: Events,
    balance_deltas: BlockBalanceDeltas,
    tick_deltas: TickDeltas,
    liquidity_changes: LiquidityChanges,
) -> Result<BlockChanges, substreams::errors::Error> {
    // Use the shared helper function to collect transaction changes
    let changes = collect_transaction_changes(
        created_pools,
        events,
        balance_deltas,
        tick_deltas,
        liquidity_changes,
    );

    // Add DCI-specific storage changes (required by DCI)
    let block_storage_changes = get_block_storage_changes(&block);

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes,
        storage_changes: block_storage_changes,
    })
}
