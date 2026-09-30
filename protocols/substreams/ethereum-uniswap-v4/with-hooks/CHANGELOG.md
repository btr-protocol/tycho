# Changelog

## v0.8.0

- Build on `ethereum-uniswap-v4-shared` 0.6.0: tick net liquidity, pool balances and the liquidity
  changes of `ModifyLiquidity` are emitted as `ChangeType::Delta`, the additive stores are
  removed, and the `map_snapshot` modules seed the pool, tick and price stores of a stream that
  starts from a state snapshot. Empty snapshot parameters, the default, leave the output as
  before.
