// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {PammBase, console2} from "./PammBase.sol";
import {LeanTychoRouter} from "@src/LeanTychoRouter.sol";
import {
    CrystalExecutor,
    CrystalExecutor__InvalidDataLength
} from "@src/executors/CrystalExecutor.sol";
import {
    CapricornExecutor,
    CapricornExecutor__InvalidCallback,
    CapricornExecutor__InvalidDataLength
} from "@src/executors/CapricornExecutor.sol";

interface ICrystalView {
    function getQuote(
        address market,
        bool isBuy,
        bool isExactInput,
        bool isCompleteFill,
        uint256 size,
        uint256 worstPrice
    ) external view returns (uint256 amountIn, uint256 amountOut);
}

interface ICapricornView {
    function quoteExactIn(address tokenIn, uint256 amountIn)
        external
        view
        returns (uint256);
    function token0() external view returns (address);
}

/// Crystal WMON/USDC book (Crystal Vault market). Base WMON, quote USDC.
contract CrystalExecutorTest is PammBase {
    address internal constant CRYSTAL =
        0x508254c838B2e936B0631440c5C6E3AB3a4a98BD;
    address internal constant MARKET =
        0x39fAE95717cfD4bdA22317F1c124660A166b6BEc;

    CrystalExecutor internal ex;

    function setUp() public {
        _fork();
        ex = new CrystalExecutor();
        _router(address(ex));
    }

    function _q(address tIn, uint256 amt) internal view returns (uint256 q) {
        bool buy = tIn == USDC;
        uint256 used;
        (used, q) = ICrystalView(CRYSTAL)
            .getQuote(MARKET, buy, true, false, amt, buy ? type(uint256).max : 0);
        assertEq(used, amt, "full fill");
    }

    function _check(address tIn, address tOut, uint256 amt) internal {
        uint256 q = _q(tIn, amt);
        assertGt(q, 0, "quote");
        (uint256 out, uint256 gas) = _swapData(
            abi.encodePacked(address(ex), tIn, tOut), CRYSTAL, tIn, tOut, amt, q
        );
        assertEq(out, q, "exec == getQuote");
        console2.log("crystal swap gas (router, singleSwap)", gas);
    }

    function testWmonToUsdc() public {
        _check(WMON, USDC, 100e18);
    }

    function testUsdcToWmon() public {
        _check(USDC, WMON, 5e6);
    }

    function testMinOutTooHighReverts() public {
        uint256 q = _q(WMON, 100e18);
        _fund(WMON, 100e18);
        vm.expectPartialRevert(LeanTychoRouter.NegativeSlippage.selector);
        vm.prank(user);
        router.singleSwap(
            100e18,
            WMON,
            USDC,
            1,
            q + 1,
            user,
            noFee,
            abi.encodePacked(address(ex), WMON, USDC)
        );
    }

    function testInvalidDataLength() public {
        vm.expectRevert(CrystalExecutor__InvalidDataLength.selector);
        ex.getTransferData(abi.encodePacked(WMON, USDC, true));
    }
}

/// Capricorn pAMM USDC/WMON pool (token0 USDC, token1 WMON).
contract CapricornExecutorTest is PammBase {
    address internal constant POOL = 0x63093325c05CD32b18034D3Ea29199fB7098E4df;

    CapricornExecutor internal ex;
    address internal t0;

    function setUp() public {
        _fork();
        ex = new CapricornExecutor();
        _router(address(ex));
        t0 = ICapricornView(POOL).token0();
    }

    function _d(address tIn, address tOut) internal view returns (bytes memory) {
        return abi.encodePacked(address(ex), POOL, tIn, tOut, tIn == t0);
    }

    function _check(address tIn, address tOut, uint256 amt) internal {
        uint256 q = ICapricornView(POOL).quoteExactIn(tIn, amt);
        assertGt(q, 0, "quote");
        (uint256 out, uint256 gas) =
            _swapData(_d(tIn, tOut), POOL, tIn, tOut, amt, q);
        assertEq(out, q, "exec == quoteExactIn");
        console2.log("capricorn swap gas (router, singleSwap)", gas);
    }

    function testWmonToUsdc() public {
        _check(WMON, USDC, 100e18);
    }

    function testUsdcToWmon() public {
        _check(USDC, WMON, 5e6);
    }

    function testMinOutTooHighReverts() public {
        uint256 q = ICapricornView(POOL).quoteExactIn(WMON, 100e18);
        _fund(WMON, 100e18);
        vm.expectPartialRevert(LeanTychoRouter.NegativeSlippage.selector);
        vm.prank(user);
        router.singleSwap(100e18, WMON, USDC, 1, q + 1, user, noFee, _d(WMON, USDC));
    }

    /// A direction flag that disagrees with tokenIn: the pool asks for the
    /// other token, the router pays tokenIn, the callback check reverts.
    function testWrongDirectionReverts() public {
        _fund(WMON, 100e18);
        vm.expectRevert();
        vm.prank(user);
        router.singleSwap(
            100e18,
            WMON,
            USDC,
            1,
            1,
            user,
            noFee,
            abi.encodePacked(address(ex), POOL, WMON, USDC, WMON != t0)
        );
    }

    function testCallbackOutsideSwapReverts() public {
        vm.expectRevert(CapricornExecutor__InvalidCallback.selector);
        ex.handleCallback(
            abi.encodeWithSelector(
                bytes4(0xf29bbfbe), int256(-1), int256(1), "", ""
            )
        );
    }

    /// A stranger calling the router's fallback mid-flight cannot be paid:
    /// outside a hop the router has no running executor.
    function testRouterCallbackOutsideHopReverts() public {
        (bool ok,) = address(router)
            .call(
                abi.encodeWithSelector(
                    bytes4(0xf29bbfbe), int256(0), int256(1), "", ""
                )
            );
        assertFalse(ok);
    }

    function testInvalidDataLength() public {
        vm.expectRevert(CapricornExecutor__InvalidDataLength.selector);
        ex.getTransferData(abi.encodePacked(POOL, WMON, USDC));
    }
}
