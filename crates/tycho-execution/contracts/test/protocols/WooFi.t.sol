// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {PammBase, console2} from "./PammBase.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {
    WooFiExecutor,
    WooFiExecutor__InvalidDataLength
} from "@src/executors/WooFiExecutor.sol";
import {LeanTychoRouter} from "@src/LeanTychoRouter.sol";

interface IWooQuery {
    function query(address, address, uint256) external view returns (uint256);
}

contract WooFiExecutorExposed is WooFiExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (address pool, address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

contract WooFiExecutorTest is PammBase {
    address internal constant WOO = 0x5520385bFcf07Ec87C4c53A7d8d65595Dff69FA4;

    WooFiExecutorExposed internal ex;

    function setUp() public {
        _fork();
        ex = new WooFiExecutorExposed();
        _router(address(ex));
    }

    function testDecodeParams() public view {
        (address p, address i, address o) =
            ex.decodeParams(abi.encodePacked(WOO, WMON, USDC));
        assertEq(p, WOO);
        assertEq(i, WMON);
        assertEq(o, USDC);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(WooFiExecutor__InvalidDataLength.selector);
        ex.decodeParams(abi.encodePacked(WOO, WMON));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType t,
            address receiver,
            address tIn,
            address tOut,
            bool toRouter
        ) = ex.getTransferData(abi.encodePacked(WOO, WMON, USDC));
        assertEq(uint8(t), uint8(TransferManager.TransferType.Transfer));
        assertEq(receiver, WOO);
        assertEq(tIn, WMON);
        assertEq(tOut, USDC);
        assertFalse(toRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(
            ex.fundsExpectedAddress(abi.encodePacked(WOO, WMON, USDC)), WOO
        );
    }

    function _check(address tIn, address tOut, uint256 amt) internal {
        uint256 q = IWooQuery(WOO).query(tIn, tOut, amt);
        assertGt(q, 0, "quote");
        (uint256 out, uint256 gas) = _swap(address(ex), WOO, tIn, tOut, amt, q);
        assertEq(out, q, "exec == query");
        console2.log("woofi swap gas (router, singleSwap)", gas);
    }

    function testWmonToUsdc() public {
        _check(WMON, USDC, 100e18);
    }

    function testUsdcToWeth() public {
        _check(USDC, WETH, 50e6);
    }

    function testCbbtcToUsdc() public {
        _check(CBBTC, USDC, 1e5);
    }

    /// base -> base routes through USDC inside the pool.
    function testWmonToWeth() public {
        _check(WMON, WETH, 100e18);
    }

    /// The Rust `woofi` encoder's bytes (`tests/pamm_encoding_tests.rs`, written to
    /// `test/assets/calldata.txt`) are the Solidity layout and execute through the router.
    function testRustEncodedVectorExecutes() public {
        bytes memory venue = loadCallDataFromFile("test_encode_woofi_wmon_usdc");
        assertEq(venue, abi.encodePacked(WOO, WMON, USDC), "layout");
        uint256 q = IWooQuery(WOO).query(WMON, USDC, 100e18);
        (uint256 out,) = _swapData(
            abi.encodePacked(address(ex), venue), WOO, WMON, USDC, 100e18, q
        );
        assertEq(out, q, "exec == query");
    }

    function testMinOutTooHighReverts() public {
        uint256 q = IWooQuery(WOO).query(WMON, USDC, 100e18);
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
            abi.encodePacked(address(ex), WOO, WMON, USDC)
        );
    }

    /// A token the pool does not list reverts and nothing stays in the pool
    /// or the router.
    function testWrongTokenReverts() public {
        _fund(USDC, 50e6);
        vm.expectRevert();
        vm.prank(user);
        router.singleSwap(
            50e6,
            USDC,
            AUSD,
            1,
            1,
            user,
            noFee,
            abi.encodePacked(address(ex), WOO, USDC, AUSD)
        );
        assertEq(IERC20(USDC).balanceOf(address(router)), 0);
    }
}
