//! Reads the initialized ticks of Uniswap V3 and V4 pools with their net liquidity, shared by the
//! concentrated-liquidity snapshot sources.

use std::sync::atomic::{AtomicBool, Ordering};

use alloy::{
    primitives::{aliases::I24, Address, Bytes as AlloyBytes, B256, U256},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        BlockId, TransactionInput, TransactionRequest,
    },
    sol,
    sol_types::SolCall,
};
use futures03::{stream, StreamExt};
use tycho_ethereum::rpc::EthereumRpcClient;

use crate::extractor::ExtractionError;

sol! {
    function tickBitmap(int16 wordPosition) external view returns (uint256);
    function ticks(int24 tick) external view returns (
        uint128 liquidityGross,
        int128 liquidityNet,
        uint256 feeGrowthOutside0X128,
        uint256 feeGrowthOutside1X128,
        int56 tickCumulativeOutside,
        uint160 secondsPerLiquidityOutsideX128,
        uint32 secondsOutside,
        bool initialized
    );

    function getTickBitmap(bytes32 poolId, int16 tick) external view returns (uint256 tickBitmap);
    function getTickLiquidity(bytes32 poolId, int24 tick)
        external
        view
        returns (uint128 liquidityGross, int128 liquidityNet);

    function scan(address pool, int24 tickSpacing, int16 fromWord, int16 toWord)
        external
        view
        returns (int24[] memory ticks, int128[] memory liquidityNet);
    function scanV4(
        address stateView,
        bytes32 poolId,
        int24 tickSpacing,
        int16 fromWord,
        int16 toWord
    ) external view returns (int24[] memory ticks, int128[] memory liquidityNet);

    struct PopulatedTick {
        int24 tick;
        int128 liquidityNet;
        uint128 liquidityGross;
    }
    function getPopulatedTicksInWord(address pool, int16 tickBitmapIndex)
        external
        view
        returns (PopulatedTick[] memory populatedTicks);
}

const MIN_TICK: i32 = -887272;
const MAX_TICK: i32 = 887272;

/// Throwaway address the tick scanner's code is injected at.
const SCANNER_ADDRESS: Address =
    alloy::primitives::address!("00000000000000000000000000000000005ca115");

/// Runtime bytecode of `lens/TickScanner.sol`, built with `lens/foundry.toml`.
pub(crate) const SCANNER_CODE: &str = include_str!("lens/TickScanner.bin");

/// Bitmap words a single scanner call covers at first; a call that fails is split.
pub(crate) const SCANNER_WORDS_PER_CALL: i32 = 1024;

/// Where a pool's ticks live.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TickSource {
    /// A Uniswap V3 pool (or fork), read directly.
    V3(Address),
    /// A Uniswap V4 pool, read through the PoolManager's `StateView`.
    V4 { state_view: Address, pool_id: B256 },
}

impl TickSource {
    /// The pool address or id, for messages.
    fn label(&self) -> String {
        match self {
            TickSource::V3(pool) => pool.to_string(),
            TickSource::V4 { pool_id, .. } => pool_id.to_string(),
        }
    }

    fn bitmap_call(&self, word: i16) -> (Address, AlloyBytes) {
        match *self {
            TickSource::V3(pool) => (
                pool,
                tickBitmapCall { wordPosition: word }
                    .abi_encode()
                    .into(),
            ),
            TickSource::V4 { state_view, pool_id } => (
                state_view,
                getTickBitmapCall { poolId: pool_id, tick: word }
                    .abi_encode()
                    .into(),
            ),
        }
    }

    fn tick_call(&self, tick: I24) -> (Address, AlloyBytes) {
        match *self {
            TickSource::V3(pool) => (pool, ticksCall { tick }.abi_encode().into()),
            TickSource::V4 { state_view, pool_id } => (
                state_view,
                getTickLiquidityCall { poolId: pool_id, tick }
                    .abi_encode()
                    .into(),
            ),
        }
    }

    fn decode_net(&self, data: &[u8]) -> Result<i128, ExtractionError> {
        match self {
            TickSource::V3(pool) => ticksCall::abi_decode_returns(data)
                .map(|info| info.liquidityNet)
                .map_err(|e| decode_error("ticks", *pool, e)),
            TickSource::V4 { .. } => getTickLiquidityCall::abi_decode_returns(data)
                .map(|info| info.liquidityNet)
                .map_err(|e| decode_error("getTickLiquidity", self.label(), e)),
        }
    }

    fn scan_call(&self, spacing: I24, from: i16, to: i16) -> Vec<u8> {
        match *self {
            TickSource::V3(pool) => {
                scanCall { pool, tickSpacing: spacing, fromWord: from, toWord: to }.abi_encode()
            }
            TickSource::V4 { state_view, pool_id } => scanV4Call {
                stateView: state_view,
                poolId: pool_id,
                tickSpacing: spacing,
                fromWord: from,
                toWord: to,
            }
            .abi_encode(),
        }
    }
}

pub(crate) fn decode_error(
    what: &str,
    pool: impl std::fmt::Display,
    e: impl std::fmt::Display,
) -> ExtractionError {
    ExtractionError::Setup(format!("Failed to decode {what} of pool {pool}: {e}"))
}

/// The `tickBitmap` word positions a pool with `tick_spacing` can use.
pub(crate) fn word_range(tick_spacing: i32) -> std::ops::RangeInclusive<i16> {
    let word = |tick: i32| (tick.div_euclid(tick_spacing) >> 8) as i16;
    word(MIN_TICK)..=word(MAX_TICK)
}

/// The initialized ticks a `tickBitmap` word marks.
pub(crate) fn ticks_in_word(word_position: i16, bitmap: U256, tick_spacing: i32) -> Vec<i32> {
    (0..256)
        .filter(|bit| bitmap.bit(*bit))
        .map(|bit| ((i32::from(word_position) << 8) + bit as i32) * tick_spacing)
        .collect()
}

/// Reads initialized ticks with the tick scanner, falling back to Multicall3 reads when the RPC
/// does not run state overrides.
pub struct TickReader {
    rpc: EthereumRpcClient,
    calls_per_request: usize,
    concurrency: usize,
    /// Uniswap's `TickLens`, used for V3 pools on the fallback path.
    tick_lens: Option<Address>,
    scanner_unsupported: AtomicBool,
}

impl TickReader {
    pub fn new(
        rpc: EthereumRpcClient,
        calls_per_request: usize,
        concurrency: usize,
        tick_lens: Option<Address>,
    ) -> Self {
        Self {
            rpc,
            calls_per_request,
            concurrency,
            tick_lens,
            scanner_unsupported: Default::default(),
        }
    }

    pub async fn multicall(
        &self,
        calls: Vec<(Address, AlloyBytes)>,
        block: BlockId,
    ) -> Result<Vec<AlloyBytes>, ExtractionError> {
        self.rpc
            .multicall(&calls, block, self.calls_per_request, self.concurrency)
            .await
            .map_err(|e| ExtractionError::Setup(format!("Snapshot read at block {block}: {e}")))
    }

    /// Returns each pool's initialized ticks with their net liquidity, in ascending order. `pools`
    /// pairs each pool with its tick spacing.
    ///
    /// Runs the tick scanner through `eth_call` state overrides: one call per 1024 bitmap words.
    /// When the RPC rejects state overrides, falls back to reading every bitmap word through
    /// Multicall3, then each initialized tick through the configured `TickLens` (V3 only), or
    /// through `ticks(i)` / `getTickLiquidity` otherwise.
    pub async fn initialized_ticks(
        &self,
        pools: &[(TickSource, i32)],
        block: BlockId,
    ) -> Result<Vec<Vec<(i32, i128)>>, ExtractionError> {
        if !self
            .scanner_unsupported
            .load(Ordering::Relaxed)
        {
            if self.scanner_supported(block).await {
                return self.scan_ticks(pools, block).await;
            }
            tracing::warn!("RPC does not run state overrides; reading ticks by bitmap word");
            self.scanner_unsupported
                .store(true, Ordering::Relaxed);
        }
        self.read_ticks(pools, block).await
    }

    fn scanner_overrides() -> Result<StateOverride, ExtractionError> {
        let code = hex::decode(SCANNER_CODE.trim())
            .map_err(|e| ExtractionError::Setup(format!("Invalid scanner code: {e}")))?;
        Ok([(SCANNER_ADDRESS, AccountOverride { code: Some(code.into()), ..Default::default() })]
            .into_iter()
            .collect())
    }

    async fn scan(
        &self,
        source: TickSource,
        spacing: i32,
        from: i16,
        to: i16,
        block: BlockId,
    ) -> Result<scanReturn, ExtractionError> {
        let pool = source.label();
        let spacing = I24::try_from(spacing).map_err(|e| decode_error("tick spacing", &pool, e))?;
        let tx = TransactionRequest::default()
            .to(SCANNER_ADDRESS)
            .input(TransactionInput::both(
                source
                    .scan_call(spacing, from, to)
                    .into(),
            ));
        let output = self
            .rpc
            .eth_call_with_state_overrides(tx, block, Self::scanner_overrides()?)
            .await
            .map_err(|e| {
                ExtractionError::Setup(format!("Tick scan of pool {pool} words {from}..={to}: {e}"))
            })?;
        // `scan` and `scanV4` return the same tuple.
        scanCall::abi_decode_returns(&output).map_err(|e| decode_error("tick scan", &pool, e))
    }

    /// Whether the RPC runs the injected scanner: an empty scan must return two empty arrays. An
    /// RPC that ignores the override returns no data, and one that rejects it returns an error.
    async fn scanner_supported(&self, block: BlockId) -> bool {
        match self
            .scan(TickSource::V3(Address::ZERO), 1, 0, -1, block)
            .await
        {
            Ok(empty) => empty.ticks.is_empty(),
            Err(err) => {
                tracing::debug!(%err, "Tick scanner probe failed");
                false
            }
        }
    }

    /// Scans each pool's bitmap words in chunks of `SCANNER_WORDS_PER_CALL`. A chunk that fails,
    /// for example by exceeding the RPC's gas cap on a dense pool, is split in half and retried;
    /// a single word that still fails is an error.
    pub(crate) async fn scan_ticks(
        &self,
        pools: &[(TickSource, i32)],
        block: BlockId,
    ) -> Result<Vec<Vec<(i32, i128)>>, ExtractionError> {
        let mut ranges = Vec::new();
        for (index, (_, spacing)) in pools.iter().enumerate() {
            let words = word_range(*spacing);
            let (first, last) = (i32::from(*words.start()), i32::from(*words.end()));
            for from in (first..=last).step_by(SCANNER_WORDS_PER_CALL as usize) {
                ranges.push((index, from, last.min(from + SCANNER_WORDS_PER_CALL - 1)));
            }
        }
        let mut ticks: Vec<Vec<(i32, i128)>> = vec![Vec::new(); pools.len()];
        while !ranges.is_empty() {
            let results: Vec<_> = stream::iter(std::mem::take(&mut ranges))
                .map(|(index, from, to)| async move {
                    let (source, spacing) = pools[index];
                    (
                        index,
                        from,
                        to,
                        self.scan(source, spacing, from as i16, to as i16, block)
                            .await,
                    )
                })
                .buffer_unordered(self.concurrency)
                .collect()
                .await;
            for (index, from, to, result) in results {
                match result {
                    Ok(scanned) => ticks[index].extend(
                        scanned
                            .ticks
                            .into_iter()
                            .map(|tick| tick.as_i32())
                            .zip(scanned.liquidityNet),
                    ),
                    Err(err) if from == to => return Err(err),
                    Err(err) => {
                        tracing::debug!(%err, "Splitting tick scan range");
                        let mid = from + (to - from) / 2;
                        ranges.extend([(index, from, mid), (index, mid + 1, to)]);
                    }
                }
            }
        }
        for pool_ticks in &mut ticks {
            pool_ticks.sort_by_key(|(tick, _)| *tick);
        }
        Ok(ticks)
    }

    /// Reads every bitmap word, then the net liquidity of each initialized tick.
    pub(crate) async fn read_ticks(
        &self,
        pools: &[(TickSource, i32)],
        block: BlockId,
    ) -> Result<Vec<Vec<(i32, i128)>>, ExtractionError> {
        let calls = pools
            .iter()
            .flat_map(|(source, spacing)| {
                word_range(*spacing).map(move |word| source.bitmap_call(word))
            })
            .collect();
        let mut bitmaps = self
            .multicall(calls, block)
            .await?
            .into_iter();
        let mut words = Vec::new();
        for (index, (source, spacing)) in pools.iter().enumerate() {
            for word in word_range(*spacing) {
                let data = bitmaps
                    .next()
                    .ok_or_else(|| decode_error("multicall", source.label(), "missing result"))?;
                // `tickBitmap` and `getTickBitmap` both return one word.
                let bitmap = tickBitmapCall::abi_decode_returns(&data)
                    .map_err(|e| decode_error("tick bitmap", source.label(), e))?;
                if !bitmap.is_zero() {
                    words.push((index, word, bitmap));
                }
            }
        }

        let mut ticks = vec![Vec::new(); pools.len()];
        let (lens_words, words): (Vec<_>, Vec<_>) = words
            .into_iter()
            .partition(|(index, _, _)| {
                self.tick_lens.is_some() && matches!(pools[*index].0, TickSource::V3(_))
            });
        if let Some(lens) = self.tick_lens {
            let calls = lens_words
                .iter()
                .map(|(index, word, _)| {
                    let TickSource::V3(pool) = pools[*index].0 else {
                        unreachable!("only V3 pools use the TickLens")
                    };
                    let call = getPopulatedTicksInWordCall { pool, tickBitmapIndex: *word };
                    (lens, call.abi_encode().into())
                })
                .collect();
            let results = self.multicall(calls, block).await?;
            for ((index, _, _), data) in lens_words.iter().zip(results) {
                let mut populated = getPopulatedTicksInWordCall::abi_decode_returns(&data)
                    .map_err(|e| {
                        decode_error("getPopulatedTicksInWord", pools[*index].0.label(), e)
                    })?;
                populated.sort_by_key(|t| t.tick);
                ticks[*index].extend(
                    populated
                        .into_iter()
                        .map(|t| (t.tick.as_i32(), t.liquidityNet)),
                );
            }
        }

        let mut owners = Vec::new();
        let mut calls = Vec::new();
        for (index, word, bitmap) in words {
            let (source, spacing) = pools[index];
            for tick in ticks_in_word(word, bitmap, spacing) {
                let tick_arg = I24::try_from(tick)
                    .map_err(|e| decode_error("tick bitmap", source.label(), e))?;
                calls.push(source.tick_call(tick_arg));
                owners.push((index, tick));
            }
        }
        for ((index, tick), data) in owners
            .into_iter()
            .zip(self.multicall(calls, block).await?)
        {
            ticks[index].push((tick, pools[index].0.decode_net(&data)?));
        }
        for pool_ticks in &mut ticks {
            pool_ticks.sort_by_key(|(tick, _)| *tick);
        }
        Ok(ticks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_word_range_covers_tick_bounds() {
        assert_eq!(word_range(60), -58..=57);
        assert_eq!(word_range(1), -3466..=3465);
    }

    #[test]
    fn test_ticks_in_word() {
        let bitmap = (U256::from(1) << 0) | (U256::from(1) << 255);
        assert_eq!(ticks_in_word(-1, bitmap, 10), vec![-2560, -10]);
        assert_eq!(ticks_in_word(0, bitmap, 10), vec![0, 2550]);
    }

    #[test]
    #[ignore = "Requires forge"]
    fn test_scanner_bytecode_matches_source() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src/extractor/bootstrap/lens");
        let out = std::env::temp_dir().join("tycho-tick-scanner");
        let status = std::process::Command::new("forge")
            .args(["build", "--root", root, "--out"])
            .arg(&out)
            .status()
            .expect("forge");
        assert!(status.success());
        let artifact: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(out.join("TickScanner.sol/TickScanner.json")).unwrap(),
        )
        .unwrap();
        let built = artifact["deployedBytecode"]["object"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x");
        assert_eq!(built, SCANNER_CODE.trim(), "rebuild lens/TickScanner.bin from its source");
        let status = std::process::Command::new("forge")
            .args(["test", "--root", root, "--out"])
            .arg(&out)
            .status()
            .expect("forge");
        assert!(status.success(), "lens/test/TickScanner.t.sol failed");
    }
}
