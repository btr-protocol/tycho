# Changelog

## 0.9.0

### Added

- `ChangeType::Delta`: an attribute or balance change whose value is a signed big-endian amount the
  indexer adds to the stored value. `BalanceChange` gains a `change` field to carry it; absolute
  balances keep the default.
- `TransactionChangesBuilder` adds a delta to an earlier change of the same attribute or balance in
  the transaction instead of replacing it.

## 0.8.1

### Fixed

- `ContractChange::is_empty` now accounts for `token_balances`, so contract changes carrying only token balance updates are no longer dropped by `TransactionChangesBuilder` (#1056).

## 0.2.0

### Updated

- Protobuf struct updated to align with recent changes in the indexer.

### Changed

- Removed the distinction between VM and native implementations. Now, there is a single implementation type that can extract both contracts and protocol state.
- Enabled the attachment of dynamic attributes to protocol components.
