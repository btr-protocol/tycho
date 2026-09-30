//! Resolution of `CHANGE_TYPE_DELTA` attributes and balances into absolute values.
//!
//! A delta is a signed big-endian integer added to the value last stored for the same key. The
//! extractor resolves deltas on the raw message, before it enters any buffer, so the reorg
//! buffer, the database and every subscriber only ever see absolute values.

use std::collections::{HashMap, HashSet};

use num_bigint::{BigInt, Sign};
use tycho_common::{
    models::{Address, AttrStoreKey, ComponentId},
    Bytes,
};
use tycho_protobuf::pb::tycho::evm::v1 as pb;

pub(crate) type AttrKey = (ComponentId, AttrStoreKey);
pub(crate) type BalanceKey = (ComponentId, Address);

/// The keys a message changes through deltas.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct DeltaKeys {
    pub attributes: HashSet<AttrKey>,
    pub balances: HashSet<BalanceKey>,
}

impl DeltaKeys {
    /// Collects every attribute and balance key that `msg` changes with a delta.
    pub(crate) fn from_message(msg: &pb::BlockChanges) -> Result<Self, String> {
        let mut keys = Self::default();
        for tx in &msg.changes {
            for entity in &tx.entity_changes {
                for attr in &entity.attributes {
                    if attr.change() == pb::ChangeType::Delta {
                        keys.attributes
                            .insert((entity.component_id.clone(), attr.name.clone()));
                    }
                }
            }
            for balance in &tx.balance_changes {
                if balance.change() == pb::ChangeType::Delta {
                    keys.balances
                        .insert((balance_component_id(balance)?, balance.token.clone().into()));
                }
            }
        }
        Ok(keys)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.attributes.is_empty() && self.balances.is_empty()
    }
}

/// Values stored for delta keys before the message. A key without an entry has no stored value.
#[derive(Debug, Default)]
pub(crate) struct PriorState {
    pub attributes: HashMap<AttrKey, Bytes>,
    pub balances: HashMap<BalanceKey, Bytes>,
}

/// Returns the ids of the components `msg` creates.
pub(crate) fn created_components(msg: &pb::BlockChanges) -> HashSet<ComponentId> {
    msg.changes
        .iter()
        .flat_map(|tx| tx.component_changes.iter())
        .filter(|component| component.change() == pb::ChangeType::Creation)
        .map(|component| component.id.clone())
        .collect()
}

/// Rewrites every delta in `msg` into the absolute value it produces, in transaction order.
///
/// An attribute delta applies to the latest value: an earlier change in `msg` if there is one,
/// otherwise the `prior` value. It becomes a `Creation` when neither exists and an `Update`
/// otherwise. A delta attribute is stored as a signed big-endian integer and is never deleted: a
/// result of zero is stored as zero.
///
/// A balance delta applies the same way to the unsigned stored balance, a missing balance counting
/// as zero. The result is written as an absolute balance.
///
/// `keys` must be the delta keys of `msg`. Errors if a balance would become negative, or if a
/// message field is malformed.
pub(crate) fn resolve(
    msg: &mut pb::BlockChanges,
    keys: &DeltaKeys,
    prior: &PriorState,
) -> Result<(), String> {
    let mut attributes: HashMap<AttrKey, Option<Bytes>> = HashMap::new();
    let mut balances: HashMap<BalanceKey, BigInt> = HashMap::new();

    for tx in &mut msg.changes {
        for entity in &mut tx.entity_changes {
            for attr in &mut entity.attributes {
                let key = (entity.component_id.clone(), attr.name.clone());
                if !keys.attributes.contains(&key) {
                    continue;
                }
                match attr.change() {
                    pb::ChangeType::Delta => {
                        let base = match attributes.get(&key) {
                            Some(latest) => latest.clone(),
                            None => prior.attributes.get(&key).cloned(),
                        };
                        let value = base
                            .as_ref()
                            .map_or_else(BigInt::default, |b| BigInt::from_signed_bytes_be(b)) +
                            BigInt::from_signed_bytes_be(&attr.value);
                        attr.value = value.to_signed_bytes_be();
                        attr.set_change(if base.is_some() {
                            pb::ChangeType::Update
                        } else {
                            pb::ChangeType::Creation
                        });
                        attributes.insert(key, Some(attr.value.clone().into()));
                    }
                    pb::ChangeType::Deletion => {
                        attributes.insert(key, None);
                    }
                    pb::ChangeType::Update | pb::ChangeType::Creation => {
                        attributes.insert(key, Some(attr.value.clone().into()));
                    }
                    pb::ChangeType::Unspecified => {}
                }
            }
        }

        for balance in &mut tx.balance_changes {
            let key = (balance_component_id(balance)?, Bytes::from(balance.token.clone()));
            if !keys.balances.contains(&key) {
                continue;
            }
            let value = if balance.change() == pb::ChangeType::Delta {
                let base = match balances.get(&key) {
                    Some(latest) => latest.clone(),
                    None => prior
                        .balances
                        .get(&key)
                        .map_or_else(BigInt::default, |b| BigInt::from_bytes_be(Sign::Plus, b)),
                };
                let value = base + BigInt::from_signed_bytes_be(&balance.balance);
                if value.sign() == Sign::Minus {
                    return Err(format!(
                        "balance delta makes the balance of token {} in component {} negative \
                         ({value}); the stored balance does not match the chain",
                        key.1, key.0
                    ));
                }
                balance.balance = value.to_bytes_be().1;
                balance.set_change(pb::ChangeType::Update);
                value
            } else {
                BigInt::from_bytes_be(Sign::Plus, &balance.balance)
            };
            balances.insert(key, value);
        }
    }
    Ok(())
}

fn balance_component_id(balance: &pb::BalanceChange) -> Result<ComponentId, String> {
    String::from_utf8(balance.component_id.clone())
        .map_err(|e| format!("balance change component id is not utf8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(name: &str, value: i64, change: pb::ChangeType) -> pb::Attribute {
        pb::Attribute {
            name: name.to_string(),
            value: BigInt::from(value).to_signed_bytes_be(),
            change: change.into(),
        }
    }

    fn balance(token: u8, value: i64, change: pb::ChangeType) -> pb::BalanceChange {
        pb::BalanceChange {
            token: vec![token],
            balance: BigInt::from(value).to_signed_bytes_be(),
            component_id: b"pool".to_vec(),
            change: change.into(),
        }
    }

    fn tx(
        attributes: Vec<pb::Attribute>,
        balances: Vec<pb::BalanceChange>,
    ) -> pb::TransactionChanges {
        pb::TransactionChanges {
            entity_changes: vec![pb::EntityChanges {
                component_id: "pool".to_string(),
                attributes,
            }],
            balance_changes: balances,
            ..Default::default()
        }
    }

    fn msg(txs: Vec<pb::TransactionChanges>) -> pb::BlockChanges {
        pb::BlockChanges { changes: txs, ..Default::default() }
    }

    fn resolve_all(m: &mut pb::BlockChanges, prior: &PriorState) -> Result<(), String> {
        let keys = DeltaKeys::from_message(m)?;
        resolve(m, &keys, prior)
    }

    fn key(name: &str) -> AttrKey {
        ("pool".to_string(), name.to_string())
    }

    fn attr_at(msg: &pb::BlockChanges, tx: usize, i: usize) -> (i64, pb::ChangeType) {
        let a = &msg.changes[tx].entity_changes[0].attributes[i];
        (i64::try_from(BigInt::from_signed_bytes_be(&a.value)).unwrap(), a.change())
    }

    #[test]
    fn test_delta_keys_skip_absolute_changes() {
        let m = msg(vec![tx(
            vec![attr("a", 1, pb::ChangeType::Delta), attr("b", 1, pb::ChangeType::Update)],
            vec![balance(1, 1, pb::ChangeType::Delta), balance(2, 1, pb::ChangeType::Unspecified)],
        )]);
        let keys = DeltaKeys::from_message(&m).unwrap();
        assert_eq!(keys.attributes, HashSet::from([key("a")]));
        assert_eq!(keys.balances, HashSet::from([("pool".to_string(), Bytes::from(vec![1]))]));
    }

    #[test]
    fn test_attribute_delta_adds_to_prior_value() {
        let mut m = msg(vec![tx(vec![attr("a", -30, pb::ChangeType::Delta)], vec![])]);
        let prior = PriorState {
            attributes: HashMap::from([(
                key("a"),
                BigInt::from(100)
                    .to_signed_bytes_be()
                    .into(),
            )]),
            ..Default::default()
        };
        resolve_all(&mut m, &prior).unwrap();
        assert_eq!(attr_at(&m, 0, 0), (70, pb::ChangeType::Update));
    }

    #[test]
    fn test_attribute_delta_without_prior_value_is_a_creation() {
        let mut m = msg(vec![tx(vec![attr("a", -5, pb::ChangeType::Delta)], vec![])]);
        resolve_all(&mut m, &PriorState::default()).unwrap();
        assert_eq!(attr_at(&m, 0, 0), (-5, pb::ChangeType::Creation));
    }

    #[test]
    fn test_attribute_delta_to_zero_is_kept() {
        let mut m = msg(vec![
            tx(vec![attr("a", 5, pb::ChangeType::Delta)], vec![]),
            tx(vec![attr("a", -5, pb::ChangeType::Delta)], vec![]),
        ]);
        resolve_all(&mut m, &PriorState::default()).unwrap();
        assert_eq!(attr_at(&m, 0, 0), (5, pb::ChangeType::Creation));
        assert_eq!(attr_at(&m, 1, 0), (0, pb::ChangeType::Update));
    }

    #[test]
    fn test_attribute_deltas_chain_through_earlier_changes_in_the_message() {
        let prior = PriorState {
            attributes: HashMap::from([(
                key("a"),
                BigInt::from(1000)
                    .to_signed_bytes_be()
                    .into(),
            )]),
            ..Default::default()
        };
        let mut m = msg(vec![
            tx(vec![attr("a", 7, pb::ChangeType::Update)], vec![]),
            tx(vec![attr("a", 3, pb::ChangeType::Delta)], vec![]),
            tx(vec![attr("a", 0, pb::ChangeType::Deletion)], vec![]),
            tx(vec![attr("a", 4, pb::ChangeType::Delta)], vec![]),
        ]);
        resolve_all(&mut m, &prior).unwrap();
        assert_eq!(attr_at(&m, 1, 0), (10, pb::ChangeType::Update));
        assert_eq!(attr_at(&m, 3, 0), (4, pb::ChangeType::Creation));
    }

    #[test]
    fn test_attribute_delta_decodes_prior_value_as_signed() {
        let prior = PriorState {
            attributes: HashMap::from([(
                key("a"),
                BigInt::from(-200)
                    .to_signed_bytes_be()
                    .into(),
            )]),
            ..Default::default()
        };
        let mut m = msg(vec![tx(vec![attr("a", 50, pb::ChangeType::Delta)], vec![])]);
        resolve_all(&mut m, &prior).unwrap();
        assert_eq!(attr_at(&m, 0, 0), (-150, pb::ChangeType::Update));
    }

    #[test]
    fn test_balance_deltas_resolve_in_order() {
        let pool_token = ("pool".to_string(), Bytes::from(vec![1]));
        let prior = PriorState {
            balances: HashMap::from([(pool_token, Bytes::from(vec![0x01, 0x00]))]),
            ..Default::default()
        };
        let mut m = msg(vec![
            tx(vec![], vec![balance(1, -56, pb::ChangeType::Delta)]),
            tx(
                vec![],
                vec![balance(1, 10, pb::ChangeType::Delta), balance(2, 9, pb::ChangeType::Delta)],
            ),
        ]);
        resolve_all(&mut m, &prior).unwrap();
        let b = |tx: usize, i: usize| {
            let b = &m.changes[tx].balance_changes[i];
            (BigInt::from_bytes_be(Sign::Plus, &b.balance), b.change())
        };
        assert_eq!(b(0, 0), (BigInt::from(200), pb::ChangeType::Update));
        assert_eq!(b(1, 0), (BigInt::from(210), pb::ChangeType::Update));
        assert_eq!(b(1, 1), (BigInt::from(9), pb::ChangeType::Update));
    }

    #[test]
    fn test_empty_values_count_as_zero() {
        let prior = PriorState {
            attributes: HashMap::from([(key("a"), Bytes::new())]),
            balances: HashMap::from([(("pool".to_string(), Bytes::from(vec![1])), Bytes::new())]),
        };
        let mut empty_delta = attr("a", 0, pb::ChangeType::Delta);
        empty_delta.value.clear();
        let mut m = msg(vec![
            tx(vec![empty_delta], vec![balance(1, 0, pb::ChangeType::Delta)]),
            tx(
                vec![attr("a", 0, pb::ChangeType::Delta)],
                vec![balance(1, 3, pb::ChangeType::Delta)],
            ),
        ]);
        resolve_all(&mut m, &prior).unwrap();
        assert_eq!(attr_at(&m, 0, 0), (0, pb::ChangeType::Update));
        assert_eq!(attr_at(&m, 1, 0), (0, pb::ChangeType::Update));
        assert_eq!(m.changes[0].balance_changes[0].balance, vec![0]);
        assert_eq!(m.changes[1].balance_changes[0].balance, vec![3]);
    }

    #[test]
    fn test_negative_balance_errors() {
        let mut m = msg(vec![tx(vec![], vec![balance(1, -1, pb::ChangeType::Delta)])]);
        let err = resolve_all(&mut m, &PriorState::default()).unwrap_err();
        assert!(err.contains("negative"), "{err}");
    }
}
