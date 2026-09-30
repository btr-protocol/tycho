// SPDX-License-Identifier: MIT
pragma solidity 0.8.33;

import {TickScanner} from "../TickScanner.sol";

contract MockPool {
    mapping(int16 => uint256) public tickBitmap;
    mapping(int24 => int128) private nets;

    function setTick(int24 tick, int24 spacing, int128 net) external {
        int24 compressed = tick / spacing;
        int16 word = int16(compressed >> 8);
        uint8 bit = uint8(uint24(compressed - int24(word) * 256));
        tickBitmap[word] |= uint256(1) << bit;
        nets[tick] = net;
    }

    function ticks(int24 tick)
        external
        view
        returns (uint128, int128, uint256, uint256, int56, uint160, uint32, bool)
    {
        int128 net = nets[tick];
        return (0, net, 0, 0, 0, 0, 0, true);
    }
}

contract TickScannerTest {
    TickScanner private scanner = new TickScanner();

    function test_scan_returns_ticks_in_order() external {
        MockPool pool = new MockPool();
        pool.setTick(-887272, 1, 5);
        pool.setTick(-256, 1, 3);
        pool.setTick(-1, 1, -2);
        pool.setTick(0, 1, 4);
        pool.setTick(887271, 1, -10);
        (int24[] memory ticks, int128[] memory nets) = scanner.scan(address(pool), 1, -3466, 3465);
        require(ticks.length == 5, "count");
        require(ticks[0] == -887272 && nets[0] == 5, "min");
        require(ticks[1] == -256 && nets[1] == 3, "word -1 bit 0");
        require(ticks[2] == -1 && nets[2] == -2, "word -1 bit 255");
        require(ticks[3] == 0 && nets[3] == 4, "zero");
        require(ticks[4] == 887271 && nets[4] == -10, "max");
    }

    function test_scan_applies_spacing_and_range() external {
        MockPool pool = new MockPool();
        pool.setTick(-600, 60, 1);
        pool.setTick(15360, 60, 2);
        (int24[] memory ticks,) = scanner.scan(address(pool), 60, -1, 0);
        require(ticks.length == 1 && ticks[0] == -600, "word range");
        (ticks,) = scanner.scan(address(pool), 60, 1, 1);
        require(ticks.length == 1 && ticks[0] == 15360, "word 1");
        (ticks,) = scanner.scan(address(pool), 60, 5, 4);
        require(ticks.length == 0, "empty");
    }

    function test_scan_rejects_bad_range() external {
        try scanner.scan(address(0), 60, 5, 3) {
            revert("accepted");
        } catch {}
    }
}
