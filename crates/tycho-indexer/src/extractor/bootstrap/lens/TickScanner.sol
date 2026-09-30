// SPDX-License-Identifier: MIT
pragma solidity 0.8.33;

interface IUniswapV3PoolTicks {
    function tickBitmap(int16 wordPosition) external view returns (uint256);
}

/// @notice Lists the initialized ticks of a Uniswap V3 pool with their net liquidity. Runs through
/// `eth_call` with its code injected by a state override; it is never deployed.
contract TickScanner {
    /// @notice Returns every initialized tick of `pool` in bitmap words `fromWord..=toWord`, in
    /// ascending order, with its `liquidityNet`. `toWord = fromWord - 1` scans nothing.
    function scan(address pool, int24 tickSpacing, int16 fromWord, int16 toWord)
        external
        view
        returns (int24[] memory ticks, int128[] memory liquidityNet)
    {
        require(tickSpacing > 0 && toWord >= fromWord - 1, "range");
        uint256 words = uint256(int256(toWord) - int256(fromWord) + 1);
        uint256[] memory bitmaps = new uint256[](words);
        uint256 count;
        for (uint256 i; i < words; ++i) {
            uint256 bitmap = IUniswapV3PoolTicks(pool).tickBitmap(int16(int256(fromWord) + int256(i)));
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
                    liquidityNet[n] = _liquidityNet(pool, tick);
                    ++n;
                }
                bitmap >>= 1;
            }
        }
    }

    /// @dev Reads the second field of `ticks(tick)`, which every Uniswap V3 fork keeps as
    /// `liquidityNet`, so the scan does not depend on the fields after it.
    function _liquidityNet(address pool, int24 tick) private view returns (int128 net) {
        (bool ok, bytes memory data) = pool.staticcall(abi.encodeWithSignature("ticks(int24)", tick));
        require(ok && data.length >= 64, "ticks");
        (, net) = abi.decode(data, (uint128, int128));
    }
}
