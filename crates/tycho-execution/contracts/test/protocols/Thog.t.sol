// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {PammBase, console2} from "./PammBase.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {
    ThogExecutor,
    ThogExecutor__InvalidDataLength
} from "@src/executors/ThogExecutor.sol";
import {LeanTychoRouter} from "@src/LeanTychoRouter.sol";

interface IThogQuote {
    function makerQuoteExactInput(address, address, uint256)
        external
        view
        returns (uint256 amountOut, uint256 lastPostedBlock);
}

contract ThogExecutorExposed is ThogExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (address amm, address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

contract ThogExecutorTest is PammBase {
    address internal constant THOG = 0x80c74517BCC2D67fFE02D3ED886796272F647210;

    ThogExecutorExposed internal ex;

    function setUp() public {
        _fork();
        ex = new ThogExecutorExposed();
        _router(address(ex));
    }

    function testDecodeParams() public view {
        (address a, address i, address o) =
            ex.decodeParams(abi.encodePacked(THOG, USDC, WMON));
        assertEq(a, THOG);
        assertEq(i, USDC);
        assertEq(o, WMON);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(ThogExecutor__InvalidDataLength.selector);
        ex.decodeParams(abi.encodePacked(THOG, USDC));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType t,
            address receiver,
            address tIn,
            address tOut,
            bool toRouter
        ) = ex.getTransferData(abi.encodePacked(THOG, USDC, WMON));
        assertEq(
            uint8(t), uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, THOG);
        assertEq(tIn, USDC);
        assertEq(tOut, WMON);
        assertFalse(toRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(
            ex.fundsExpectedAddress(abi.encodePacked(THOG, USDC, WMON)),
            address(this)
        );
    }

    function _check(address tIn, address tOut, uint256 amt) internal {
        (uint256 q,) = IThogQuote(THOG).makerQuoteExactInput(tIn, tOut, amt);
        assertGt(q, 0, "quote");
        // forge's default tx.gasprice is 0: below the 1.5x basefee threshold,
        // so the execution defense is cold and exec must equal the quote.
        (uint256 out, uint256 gas) = _swap(address(ex), THOG, tIn, tOut, amt, q);
        assertEq(out, q, "exec == quote");
        console2.log("thog swap gas (router, singleSwap)", gas);
    }

    function testUsdcToWmon() public {
        _check(USDC, WMON, 1e6);
    }

    function testWmonToUsdc() public {
        _check(WMON, USDC, 1e18);
    }

    function testCbbtcToWeth() public {
        _check(CBBTC, WETH, 1e5);
    }

    function testWbtcToUsdc() public {
        _check(WBTC, USDC, 1e5);
    }

    /// Execution defense: the guide documents 0 bp at gas price <= 1.5x
    /// basefee and 6/8/12 bp above 1.5x/2x/2.5x. Logs what the fork sees per
    /// multiple; exec never exceeds the view quote and stays within 12 bp.
    function testGasPriceDefense() public {
        (uint256 q,) = IThogQuote(THOG).makerQuoteExactInput(USDC, WMON, 1e7);
        uint256[4] memory mult = [uint256(10), 16, 21, 30];
        vm.fee(1 gwei);
        for (uint256 i = 0; i < 4; ++i) {
            uint256 snap = vm.snapshotState();
            vm.txGasPrice(mult[i] * 1 gwei / 10);
            (uint256 out,) = _swap(address(ex), THOG, USDC, WMON, 1e7, 1);
            console2.log("gas price x0.1 basefee", mult[i]);
            console2.log("  shortfall ppm", (q - out) * 1e6 / q);
            assertLe(out, q);
            assertGe(out, q * 9988 / 10000, "within 12 bp");
            vm.revertToState(snap);
        }
    }

    function testMinOutTooHighReverts() public {
        (uint256 q,) = IThogQuote(THOG).makerQuoteExactInput(USDC, WMON, 1e6);
        _fund(USDC, 1e6);
        vm.expectPartialRevert(LeanTychoRouter.NegativeSlippage.selector);
        vm.prank(user);
        router.singleSwap(
            1e6,
            USDC,
            WMON,
            1,
            q + 1,
            user,
            noFee,
            abi.encodePacked(address(ex), THOG, USDC, WMON)
        );
    }

    /// A token the AMM does not list (and a pair with itself) reverts.
    function testWrongTokenReverts() public {
        _fund(USDC, 1e6);
        vm.expectRevert();
        vm.prank(user);
        router.singleSwap(
            1e6,
            USDC,
            makeAddr("unlisted"),
            1,
            1,
            user,
            noFee,
            abi.encodePacked(address(ex), THOG, USDC, makeAddr("unlisted"))
        );
    }
}
