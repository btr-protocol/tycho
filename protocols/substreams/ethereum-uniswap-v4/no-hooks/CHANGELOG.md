# Changelog

## v0.4.4

- Add a block filter to `monad-uniswap-v4-no-hooks.yaml`, using the `index_events` block index
  imported from StreamingFast `ethereum-common` v0.3.3. `map_pools_created` runs only on blocks
  with a `Initialize` log; `map_events` and `map_protocol_changes` only on blocks with a log whose
  topic0 is `Initialize`, `Swap`, `ModifyLiquidity` or `ProtocolFeeUpdated`. The modules fed only
  by these skip with them, so blocks without a relevant log are neither processed nor billed.
  Output on matching blocks and the module code are unchanged.

## v0.4.3

- Add the Monad Uniswap V4 no-hooks manifest (PoolManager `0x188d586ddcf52439676ca21a244753fa19f9ea8e`, first `Initialize` at block 30255261).

## v0.4.2

- Add the Robinhood Chain Uniswap V4 no-hooks manifest.
