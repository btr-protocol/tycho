use crate::pb::pancakeswap::v3::{
    events::{pool_event, PoolEvent},
    Events, LiquidityChangeType, LiquidityChanges, TickDeltas,
};
use itertools::Itertools;
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr, vec};
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

type PoolAddress = Vec<u8>;

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct Params {
    /// Protocol fee, per direction, that `initialize` sets on every pool. PancakeSwap V3 picks it
    /// by fee tier; forks that use one constant for every tier set it here.
    default_protocol_fee: Option<u64>,
}

/// Emits tick net liquidity, liquidity changes from mints and burns, and balances as
/// `ChangeType::Delta`, so the output does not depend on the block the stream starts at. Swap
/// and initialization values are absolute.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::Block,
    created_pools: BlockChanges,
    events: Events,
    balance_deltas: BlockBalanceDeltas,
    tick_deltas: TickDeltas,
    liquidity_changes: LiquidityChanges,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params: Params = serde_qs::from_str(&params)
        .map_err(|err| anyhow::anyhow!("Invalid map_protocol_changes params {params:?}: {err}"))?;
    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes(
            &params,
            created_pools,
            events,
            balance_deltas,
            tick_deltas,
            liquidity_changes,
        ),
        ..Default::default()
    })
}

/// Groups a block's pool changes by transaction, in transaction order.
pub(crate) fn transaction_changes(
    params: &Params,
    created_pools: BlockChanges,
    events: Events,
    balance_deltas: BlockBalanceDeltas,
    tick_deltas: TickDeltas,
    liquidity_changes: LiquidityChanges,
) -> Vec<TransactionChanges> {
    // We merge contract changes by transaction (identified by transaction index) making it easy to
    //  sort them at the very end.
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    // Add created pools to the tx_changes_map
    for change in created_pools.changes.into_iter() {
        let tx = change.tx.as_ref().unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        change
            .component_changes
            .iter()
            .for_each(|c| {
                builder.add_protocol_component(c);
            });
        change
            .entity_changes
            .iter()
            .for_each(|ec| {
                builder.add_entity_change(ec);
            });
        change
            .balance_changes
            .iter()
            .for_each(|bc| {
                builder.add_balance_change(bc);
            });
    }

    for delta in balance_deltas
        .balance_deltas
        .into_iter()
        .sorted_by_key(|delta| delta.ord)
    {
        let tx = delta.tx.unwrap();
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_balance_change(&BalanceChange {
                token: delta.token,
                balance: delta.delta,
                component_id: delta.component_id,
                change: ChangeType::Delta.into(),
            });
    }

    for tick_delta in tick_deltas.deltas {
        let tx = tick_delta.transaction.unwrap();
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()))
            .add_entity_change(&EntityChanges {
                component_id: tick_delta.pool_address.to_hex(),
                attributes: vec![Attribute {
                    name: format!("ticks/{}/net-liquidity", tick_delta.tick_index),
                    value: tick_delta.liquidity_net_delta,
                    change: ChangeType::Delta.into(),
                }],
            });
    }

    // Changes arrive in ordinal order, so a swap's absolute liquidity replaces the deltas before
    // it and later mints and burns add to it.
    for change in liquidity_changes.changes {
        let change_type = match change.change_type() {
            LiquidityChangeType::Delta => ChangeType::Delta,
            LiquidityChangeType::Absolute => ChangeType::Update,
        };
        let tx = change.transaction.unwrap();
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()))
            .add_entity_change(&EntityChanges {
                component_id: change.pool_address.to_hex(),
                attributes: vec![Attribute {
                    name: "liquidity".to_string(),
                    value: change.value,
                    change: change_type.into(),
                }],
            });
    }

    // Insert others changes
    events
        .pool_events
        .into_iter()
        .flat_map(|event| event_to_attributes_updates(event, params))
        .for_each(|(tx, pool_address, attr)| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            builder.add_entity_change(&EntityChanges {
                component_id: pool_address.to_hex(),
                attributes: vec![attr],
            });
        });

    transaction_changes
        .drain()
        .sorted_unstable_by_key(|(index, _)| *index)
        .filter_map(|(_, builder)| builder.build())
        .collect()
}

fn event_to_attributes_updates(
    event: PoolEvent,
    params: &Params,
) -> Vec<(Transaction, PoolAddress, Attribute)> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Initialize(initalize) => {
            let (zero_to_one, one_to_zero) = default_protocol_fees(event.fee, params);
            vec![
                (
                    event
                        .transaction
                        .as_ref()
                        .unwrap()
                        .into(),
                    hex::decode(&event.pool_address).unwrap(),
                    Attribute {
                        name: "sqrt_price_x96".to_string(),
                        value: BigInt::from_str(&initalize.sqrt_price)
                            .unwrap()
                            .to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    },
                ),
                (
                    event
                        .transaction
                        .as_ref()
                        .unwrap()
                        .into(),
                    hex::decode(&event.pool_address).unwrap(),
                    Attribute {
                        name: "tick".to_string(),
                        value: BigInt::from(initalize.tick).to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    },
                ),
                (
                    event
                        .transaction
                        .as_ref()
                        .unwrap()
                        .into(),
                    hex::decode(&event.pool_address).unwrap(),
                    Attribute {
                        name: "protocol_fees/zero2one".to_string(),
                        value: BigInt::from(zero_to_one).to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    },
                ),
                (
                    event.transaction.unwrap().into(),
                    hex::decode(event.pool_address).unwrap(),
                    Attribute {
                        name: "protocol_fees/one2zero".to_string(),
                        value: BigInt::from(one_to_zero).to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    },
                ),
            ]
        }
        pool_event::Type::Swap(swap) => vec![
            (
                event
                    .transaction
                    .as_ref()
                    .unwrap()
                    .into(),
                hex::decode(&event.pool_address).unwrap(),
                Attribute {
                    name: "sqrt_price_x96".to_string(),
                    value: BigInt::from_str(&swap.sqrt_price)
                        .unwrap()
                        .to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            ),
            (
                event.transaction.unwrap().into(),
                hex::decode(event.pool_address).unwrap(),
                Attribute {
                    name: "tick".to_string(),
                    value: BigInt::from(swap.tick).to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            ),
        ],
        pool_event::Type::SetFeeProtocol(sfp) => vec![
            (
                event
                    .transaction
                    .as_ref()
                    .unwrap()
                    .into(),
                hex::decode(&event.pool_address).unwrap(),
                Attribute {
                    name: "protocol_fees/zero2one".to_string(),
                    value: BigInt::from(sfp.fee_protocol_0_new).to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            ),
            (
                event.transaction.unwrap().into(),
                hex::decode(event.pool_address).unwrap(),
                Attribute {
                    name: "protocol_fees/one2zero".to_string(),
                    value: BigInt::from(sfp.fee_protocol_1_new).to_signed_bytes_be(),
                    change: ChangeType::Update.into(),
                },
            ),
        ],
        _ => vec![],
    }
}

fn default_protocol_fees(fee: u64, params: &Params) -> (u64, u64) {
    match params.default_protocol_fee {
        Some(protocol_fee) => (protocol_fee, protocol_fee),
        None => fee_to_default_protocol_fees(fee),
    }
}

// Map the pool fee to the default protocol fees.
// For the reference implementation see https://github.com/pancakeswap/pancake-v3-contracts/blob/5cc479f0c5a98966c74d94700057b8c3ca629afd/projects/v3-core/contracts/PancakeV3Pool.sol#L298-L306
fn fee_to_default_protocol_fees(fee: u64) -> (u64, u64) {
    match fee {
        100 => (3300, 3300),
        500 => (3400, 3400),
        2500 => (3200, 3200),
        10000 => (3200, 3200),
        _ => panic!(
            "Unexpected fee value {fee}: PancakeSwap V3 sets no default protocol fee for it. A fork \
             that uses one default for every fee tier must pass `default_protocol_fee` to \
             map_protocol_changes"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_protocol_fees_follow_pancakeswap_fee_tiers_without_params() {
        let params: Params = serde_qs::from_str("").unwrap();

        assert_eq!(params, Params::default());
        assert_eq!(default_protocol_fees(100, &params), (3300, 3300));
        assert_eq!(default_protocol_fees(500, &params), (3400, 3400));
        assert_eq!(default_protocol_fees(2500, &params), (3200, 3200));
        assert_eq!(default_protocol_fees(10000, &params), (3200, 3200));
    }

    #[test]
    fn test_default_protocol_fee_param_applies_to_every_fee() {
        let params: Params = serde_qs::from_str("default_protocol_fee=1000").unwrap();

        for fee in [50, 100, 200, 500, 2000, 10000, 30000] {
            assert_eq!(default_protocol_fees(fee, &params), (1000, 1000));
        }
    }

    #[test]
    #[should_panic(expected = "Unexpected fee value 50")]
    fn test_unknown_fee_without_param_panics() {
        default_protocol_fees(50, &Params::default());
    }

    #[test]
    fn test_invalid_default_protocol_fee_is_rejected() {
        assert!(serde_qs::from_str::<Params>("default_protocol_fee=ten").is_err());
    }
}
