# Changelog

## v0.1.0

- Index Hanji on Monad as `vm:hanji`. One component per fast-quoter proxy (the taker entry of a
  market), created from the proxy factory's creation event with the market's tokens from the CLOB
  factory's `OnchainCLOBCreated`.
- Quote-path state is traced, not decoded: each component registers DCI entry points for a
  zero-fill order per side on the proxy (quoters, oracle adapter, Pyth, LP manager, market,
  tries), each token's `balanceOf` the LP manager and the market plus the LP manager's allowance
  to the market (the token slots a fill moves), the market watchdog (only called on fills) and the
  alternate fast quoters (only used for whitelisted origins). Every block's storage changes are
  emitted so the DCI keeps traced contracts current, including the maker quotes a relay re-pushes
  to the fast quoter about every block and Pyth price updates.
- Requires extended blocks.
