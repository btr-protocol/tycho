# Changelog

## 0.2.0

- Streamable from a snapshot: `map_snapshot_<i>` modules seed `store_markets`, `store_order_sizes`
  and `store_order_meta` with the markets and resting orders of a state at block N.
- Levels `a/<price>`, `b/<price>` and balances are emitted as deltas; `store_levels` and
  `store_balances` are gone.
- Resting orders are kept as `o/<id>` attributes (price u32, is_buy u8, size u96, big-endian),
  deleted when the order leaves the book, so a restart can rebuild the seed from the DB.
- An event that resizes an order the stores do not know fails the block.

## 0.1.1

- Filter `map_markets` and `map_events` with the `ethereum-common` v0.3.3 `index_events` block index
  (topic0 of `MarketRegistered` and of the six order book events). `map_protocol_changes` stays
  unfiltered because it reads market storage writes that may carry no market log.

## 0.1.0

- Kuru markets on Monad: levels from market events (shared `kuru-book` rules), vault/state/fees from market storage, book depth as balances.
