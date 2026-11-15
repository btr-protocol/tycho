// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {Test, console2} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {LeanTychoRouter, ClientFeeParams} from "@src/LeanTychoRouter.sol";

/// Shared Monad fork setup for the prop-AMM executor tests: every swap goes
/// through a LeanTychoRouter whose only executor is the one under test, from
/// a funded user, as the arb keeper's TYCHO leg would call it.
/// MONAD_RPC_URL=https://rpc3.monad.xyz forge test --use solar --network monad
abstract contract PammBase is Test {
    address internal constant USDC = 0x754704Bc059F8C67012fEd69BC8A327a5aafb603;
    address internal constant AUSD = 0x00000000eFE302BEAA2b3e6e1b18d08D69a9012a;
    address internal constant WMON = 0x3bd359C1119dA7Da1D913D1C4D2B7c461115433A;
    address internal constant WETH = 0xEE8c0E9f1BFFb4Eb878d8f15f368A02a35481242;
    address internal constant WBTC = 0x0555E30da8f98308EdB960aa94C0Db47230d2B9c;
    address internal constant CBBTC = 0xd18B7EC58Cdf4876f6AFebd3Ed1730e4Ce10414b;

    LeanTychoRouter internal router;
    address internal user = makeAddr("user");
    ClientFeeParams internal noFee;

    function _fork() internal {
        vm.createSelectFork(
            vm.envOr("MONAD_RPC_URL", string("https://rpc3.monad.xyz")),
            vm.envOr("FORK_BLOCK", uint256(122_522_000))
        );
    }

    function _router(address executor) internal {
        address[] memory e = new address[](1);
        e[0] = executor;
        router = new LeanTychoRouter(e);
    }

    function _fund(address tIn, uint256 amt) internal {
        deal(tIn, user, amt);
        vm.prank(user);
        IERC20(tIn).approve(address(router), amt);
    }

    /// Swaps `amt` of `tIn` through the router; returns (out, gas).
    function _swap(
        address executor,
        address venue,
        address tIn,
        address tOut,
        uint256 amt,
        uint256 minOut
    ) internal returns (uint256 out, uint256 gas) {
        _fund(tIn, amt);
        bytes memory swapData = abi.encodePacked(executor, venue, tIn, tOut);
        vm.startPrank(user);
        uint256 b0 = IERC20(tOut).balanceOf(user);
        gas = gasleft();
        router.singleSwap(amt, tIn, tOut, 1, minOut, user, noFee, swapData);
        gas -= gasleft();
        vm.stopPrank();
        out = IERC20(tOut).balanceOf(user) - b0;
        assertEq(IERC20(tIn).balanceOf(address(router)), 0, "router tokenIn");
        assertEq(IERC20(tOut).balanceOf(address(router)), 0, "router tokenOut");
        assertEq(IERC20(tIn).allowance(address(router), venue), 0, "allowance");
    }
}
