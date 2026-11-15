// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {PammBase, console2} from "./PammBase.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {
    PoeExecutor,
    PoeExecutor__InvalidCallback,
    PoeExecutor__InvalidDataLength
} from "@src/executors/PoeExecutor.sol";
import {LeanTychoRouter} from "@src/LeanTychoRouter.sol";

interface IPoePoolView {
    function getTokens() external view returns (address x, address y);
    function getQuote(bool swapXtoY, uint256 amountIn)
        external
        view
        returns (
            uint256 amountOut,
            uint256 actualAmountIn,
            uint256 feeIn,
            uint256 feeOut
        );
}

contract PoeExecutorExposed is PoeExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (address pool, address tokenIn, address tokenOut, bool xToY)
    {
        return _decodeData(data);
    }
}

/// LFJ POE WMON/USDC pool: the only POE pool among our tokens on Monad.
contract PoeExecutorTest is PammBase {
    address internal constant POOL = 0x02A8A16613a421EabaD6861fF6d8159f6D5EDB8f;

    PoeExecutorExposed internal ex;
    address internal tokenX;

    function setUp() public {
        _fork();
        ex = new PoeExecutorExposed();
        _router(address(ex));
        (tokenX,) = IPoePoolView(POOL).getTokens();
    }

    function _d(address tIn, address tOut) internal view returns (bytes memory) {
        return abi.encodePacked(address(ex), POOL, tIn, tOut, tIn == tokenX);
    }

    function testDecodeParams() public view {
        (address p, address i, address o, bool xy) =
            ex.decodeParams(abi.encodePacked(POOL, WMON, USDC, true));
        assertEq(p, POOL);
        assertEq(i, WMON);
        assertEq(o, USDC);
        assertTrue(xy);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(PoeExecutor__InvalidDataLength.selector);
        ex.decodeParams(abi.encodePacked(POOL, WMON, USDC));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType t,
            address receiver,
            address tIn,
            address tOut,
            bool toRouter
        ) = ex.getTransferData(abi.encodePacked(POOL, WMON, USDC, true));
        assertEq(uint8(t), uint8(TransferManager.TransferType.None));
        assertEq(receiver, address(0));
        assertEq(tIn, WMON);
        assertEq(tOut, USDC);
        assertFalse(toRouter);
    }

    function testCallbackOutsideSwapReverts() public {
        vm.expectRevert(PoeExecutor__InvalidCallback.selector);
        ex.handleCallback(
            abi.encodeWithSelector(
                bytes4(0xfa483e72), int256(1), int256(-1), bytes("")
            )
        );
    }

    function _check(address tIn, address tOut, uint256 amt) internal {
        bool xy = tIn == tokenX;
        (uint256 q, uint256 actualIn,,) =
            IPoePoolView(POOL).getQuote(xy, amt);
        assertGt(q, 0, "quote");
        assertEq(actualIn, amt, "full fill");
        (uint256 out, uint256 gas) =
            _swapData(_d(tIn, tOut), POOL, tIn, tOut, amt, q);
        assertEq(out, q, "exec == getQuote");
        console2.log("poe swap gas (router, singleSwap)", gas);
    }

    function testWmonToUsdc() public {
        _check(WMON, USDC, 100e18);
    }

    function testUsdcToWmon() public {
        _check(USDC, WMON, 5e6);
    }

    /// The Rust `lfj_poe` encoder's bytes (`tests/pamm_encoding_tests.rs`, written to
    /// `test/assets/calldata.txt`; X = WMON) are the Solidity layout, direction flag
    /// included, and execute through the router in both directions.
    function testRustEncodedVectorsExecute() public {
        assertEq(tokenX, WMON, "pool X");
        bytes memory xy = loadCallDataFromFile("test_encode_poe_wmon_usdc");
        assertEq(xy, abi.encodePacked(POOL, WMON, USDC, true), "x->y layout");
        (uint256 q,,,) = IPoePoolView(POOL).getQuote(true, 100e18);
        (uint256 out,) = _swapData(
            abi.encodePacked(address(ex), xy), POOL, WMON, USDC, 100e18, q
        );
        assertEq(out, q, "x->y exec == getQuote");

        bytes memory yx = loadCallDataFromFile("test_encode_poe_usdc_wmon");
        assertEq(yx, abi.encodePacked(POOL, USDC, WMON, false), "y->x layout");
        (q,,,) = IPoePoolView(POOL).getQuote(false, 5e6);
        (out,) = _swapData(
            abi.encodePacked(address(ex), yx), POOL, USDC, WMON, 5e6, q
        );
        assertEq(out, q, "y->x exec == getQuote");
    }

    function testMinOutTooHighReverts() public {
        (uint256 q,,,) = IPoePoolView(POOL).getQuote(WMON == tokenX, 100e18);
        _fund(WMON, 100e18);
        vm.expectPartialRevert(LeanTychoRouter.NegativeSlippage.selector);
        vm.prank(user);
        router.singleSwap(
            100e18, WMON, USDC, 1, q + 1, user, noFee, _d(WMON, USDC)
        );
    }

    /// Wrong direction flag: the pool asks for the other token, the router
    /// pays tokenIn, the pool's balance check reverts.
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
            abi.encodePacked(address(ex), POOL, WMON, USDC, WMON != tokenX)
        );
    }

    /// A token the pool does not hold reverts.
    function testWrongTokenReverts() public {
        _fund(WETH, 1e15);
        vm.expectRevert();
        vm.prank(user);
        router.singleSwap(
            1e15,
            WETH,
            USDC,
            1,
            1,
            user,
            noFee,
            abi.encodePacked(address(ex), POOL, WETH, USDC, true)
        );
    }
}
