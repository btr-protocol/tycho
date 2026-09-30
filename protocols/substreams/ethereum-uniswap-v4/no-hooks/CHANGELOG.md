# Changelog

## v0.5.0

- Emit tick net liquidity, pool balances and the liquidity changes of `ModifyLiquidity` as
  `ChangeType::Delta` instead of absolute values read from additive stores; the indexer adds each
  delta to the stored value. Replayed from the PoolManager's deployment, the stored values match
  the previous version. `store_pools_balances`, `store_ticks_liquidity` and `store_liquidity` are
  removed.
- Add `map_snapshot` and its chunks `map_snapshot_0..7`. With empty parameters, the default, they
  emit nothing. An indexer bootstrapped from a state snapshot at block N passes the snapshot's
  pools with their tick and sqrtPriceX96, sets every module's `initialBlock` to N+1, and
  `store_pools`, `store_pool_current_tick` and `store_pool_current_sqrt_price` start from them.
- A liquidity change of a pool whose tick or price is unknown fails the module instead of reading
  zero. This only happens when a manifest starts after the pool's `Initialize` without a snapshot.
- A tick whose net liquidity returns to zero keeps a zero attribute instead of being deleted.
- Depend on the in-repo `tycho-substreams` and `substreams-helper` crates.

## v0.4.3

- Add the Monad Uniswap V4 no-hooks manifest (PoolManager `0x188d586ddcf52439676ca21a244753fa19f9ea8e`, first `Initialize` at block 30255261).

## v0.4.2

- Add the Robinhood Chain Uniswap V4 no-hooks manifest.
