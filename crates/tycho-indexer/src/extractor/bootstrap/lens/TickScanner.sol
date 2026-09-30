// SPDX-License-Identifier: MIT
pragma solidity 0.8.33;

interface IUniswapV3PoolTicks {
    function tickBitmap(int16 wordPosition) external view returns (uint256);
}

interface IStateView {
    function getTickBitmap(bytes32 poolId, int16 tick) external view returns (uint256);
    function getTickLiquidity(bytes32 poolId, int24 tick)
        external
        view
        returns (uint128 liquidityGross, int128 liquidityNet);
}

/// @notice Lists the initialized ticks of a Uniswap V3 pool, or of a Uniswap V4 pool through its
/// `StateView`, with their net liquidity. Runs through `eth_call` with its code injected by a state
/// override; it is never deployed.
contract TickScanner {
    /// @notice Returns every initialized tick of V3 `pool` in bitmap words `fromWord..=toWord`, in
    /// ascending order, with its `liquidityNet`. `toWord = fromWord - 1` scans nothing.
    function scan(address pool, int24 tickSpacing, int16 fromWord, int16 toWord)
        external
        view
        returns (int24[] memory ticks, int128[] memory liquidityNet)
    {
        return _scan(pool, bytes32(0), tickSpacing, fromWord, toWord);
    }

    /// @notice `scan` for the V4 pool `poolId`, read through `stateView`.
    function scanV4(address stateView, bytes32 poolId, int24 tickSpacing, int16 fromWord, int16 toWord)
        external
        view
        returns (int24[] memory ticks, int128[] memory liquidityNet)
    {
        require(poolId != bytes32(0), "pool id");
        return _scan(stateView, poolId, tickSpacing, fromWord, toWord);
    }

    /// @dev `poolId == 0` reads V3 pool `target`, otherwise V4 pool `poolId` through `target`.
    function _scan(address target, bytes32 poolId, int24 tickSpacing, int16 fromWord, int16 toWord)
        private
        view
        returns (int24[] memory ticks, int128[] memory liquidityNet)
    {
        require(tickSpacing > 0 && toWord >= fromWord - 1, "range");
        uint256 words = uint256(int256(toWord) - int256(fromWord) + 1);
        uint256[] memory bitmaps = new uint256[](words);
        uint256 count;
        for (uint256 i; i < words; ++i) {
            int16 word = int16(int256(fromWord) + int256(i));
            uint256 bitmap = poolId == bytes32(0)
                ? IUniswapV3PoolTicks(target).tickBitmap(word)
                : IStateView(target).getTickBitmap(poolId, word);
            bitmaps[i] = bitmap;
            for (; bitmap != 0; ++count) {
                bitmap &= bitmap - 1;
            }
        }

        ticks = new int24[](count);
        liquidityNet = new int128[](count);
        uint256 n;
        for (uint256 i; i < words; ++i) {
            uint256 bitmap = bitmaps[i];
            for (uint256 bit; bitmap != 0; ++bit) {
                if (bitmap & 1 == 1) {
                    int24 tick = int24((int256(fromWord) + int256(i)) * 256 + int256(bit)) * tickSpacing;
                    ticks[n] = tick;
                    liquidityNet[n] = _liquidityNet(target, poolId, tick);
                    ++n;
                }
                bitmap >>= 1;
            }
        }
    }

    /// @dev V3: the second field of `ticks(tick)`, which every Uniswap V3 fork keeps as
    /// `liquidityNet`, so the scan does not depend on the fields after it.
    function _liquidityNet(address target, bytes32 poolId, int24 tick) private view returns (int128 net) {
        if (poolId != bytes32(0)) {
            (, net) = IStateView(target).getTickLiquidity(poolId, tick);
            return net;
        }
        (bool ok, bytes memory data) = target.staticcall(abi.encodeWithSignature("ticks(int24)", tick));
        require(ok && data.length >= 64, "ticks");
        (, net) = abi.decode(data, (uint128, int128));
    }
}
