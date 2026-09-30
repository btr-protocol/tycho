// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import {
    IERC20,
    SafeERC20
} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";
import "src/hanji/HanjiSwapAdapter.sol";
import "src/interfaces/ISwapAdapterTypes.sol";

/// Run with `forge test -n monad --match-contract HanjiSwapAdapterTest` and
/// `MONAD_RPC_URL` set.
contract HanjiSwapAdapterTest is Test, ISwapAdapterTypes {
    using SafeERC20 for IERC20;

    HanjiSwapAdapter adapter;

    /// MON/USDC fast-quoter proxy and its market.
    address constant PROXY = 0x1aeD222dda944a87703c918745b11bE13f8eEf10;
    address constant WMON = 0x3bd359C1119dA7Da1D913D1C4D2B7c461115433A;
    address constant USDC = 0x754704Bc059F8C67012fEd69BC8A327a5aafb603;
    bytes32 constant POOL_ID = bytes32(bytes20(PROXY));

    function setUp() public {
        // A block where the fast quoter is quoting (it goes quiet at times;
        // the proxy then fills only the resting book).
        vm.createSelectFork(vm.rpcUrl("monad"), 109038858);
        adapter = new HanjiSwapAdapter();
        vm.label(address(adapter), "HanjiSwapAdapter");
        vm.label(PROXY, "MonUsdcProxy");
    }

    function testGetTokens() public view {
        address[] memory tokens = adapter.getTokens(POOL_ID);
        assertEq(tokens[0], WMON);
        assertEq(tokens[1], USDC);
    }

    function testCapabilitiesAreSellOnly() public view {
        Capability[] memory caps = adapter.getCapabilities(POOL_ID, WMON, USDC);
        assertEq(caps.length, 1);
        assertEq(uint256(caps[0]), uint256(Capability.SellOrder));
    }

    function testSwapSellBaseMatchesProxy() public {
        _assertSwapMatchesProxy(WMON, USDC, 1000 ether);
        _assertSwapMatchesProxy(WMON, USDC, 25 ether + 1);
    }

    function testSwapSellQuoteMatchesProxy() public {
        _assertSwapMatchesProxy(USDC, WMON, 30e6);
        _assertSwapMatchesProxy(USDC, WMON, 1234e6 + 7);
    }

    function testSwapBelowOneShareReverts() public {
        deal(WMON, address(this), 1 ether);
        IERC20(WMON).forceApprove(address(adapter), type(uint256).max);
        vm.expectRevert(abi.encodeWithSelector(TooSmall.selector, 1 ether));
        adapter.swap(POOL_ID, WMON, USDC, OrderSide.Sell, 1 ether - 1);
    }

    function testSwapBeyondBookReverts() public {
        uint256 amount = 1_000_000_000 ether;
        deal(WMON, address(this), amount);
        IERC20(WMON).forceApprove(address(adapter), type(uint256).max);
        vm.expectRevert();
        adapter.swap(POOL_ID, WMON, USDC, OrderSide.Sell, amount);
    }

    function testBuyOrderNotImplemented() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                NotImplemented.selector, "HanjiSwapAdapter: exact-input only"
            )
        );
        adapter.swap(POOL_ID, WMON, USDC, OrderSide.Buy, 1 ether);
    }

    function testGetLimitsFillable() public {
        _fund(WMON, 1e30);
        uint256[] memory limits = adapter.getLimits(POOL_ID, WMON, USDC);
        assertGt(limits[0], 0);
        assertGt(limits[1], 0);
        // 1% of the sell limit, the size spot pricing probes, fills.
        adapter.swap(POOL_ID, WMON, USDC, OrderSide.Sell, limits[0] / 100);

        _fund(USDC, 1e20);
        limits = adapter.getLimits(POOL_ID, USDC, WMON);
        assertGt(limits[0], 0);
        adapter.swap(POOL_ID, USDC, WMON, OrderSide.Sell, limits[0] / 100);
    }

    /// The adapter's output equals the proxy's own quote for the same order
    /// and what the adapter actually pays out, and it charges exactly the
    /// executed input.
    function _assertSwapMatchesProxy(
        address sellToken,
        address buyToken,
        uint256 amount
    ) internal {
        uint256 snapshot = vm.snapshot();
        uint256 expected = _proxyQuote(sellToken, amount);
        vm.revertTo(snapshot);

        _fund(sellToken, amount);
        uint256 sellBefore = IERC20(sellToken).balanceOf(address(this));
        uint256 buyBefore = IERC20(buyToken).balanceOf(address(this));
        Trade memory trade =
            adapter.swap(POOL_ID, sellToken, buyToken, OrderSide.Sell, amount);

        assertEq(trade.calculatedAmount, expected, "adapter != proxy quote");
        assertEq(
            IERC20(buyToken).balanceOf(address(this)) - buyBefore, expected
        );
        assertLe(
            sellBefore - IERC20(sellToken).balanceOf(address(this)), amount
        );
        assertEq(IERC20(sellToken).balanceOf(address(adapter)), 0);
        assertEq(IERC20(buyToken).balanceOf(address(adapter)), 0);
    }

    /// The proxy's quote path called directly by a taker, in buy-token units.
    function _proxyQuote(address sellToken, uint256 amount)
        internal
        returns (uint256)
    {
        address taker = makeAddr("taker");
        deal(sellToken, taker, amount);
        vm.startPrank(taker);
        IERC20(sellToken).forceApprove(PROXY, amount);
        uint256 out;
        if (sellToken == WMON) {
            (, uint128 shares, uint128 value, uint128 fee) = IHanjiProxy(PROXY)
                .placeOrder(
                    true,
                    uint128(amount / 1 ether),
                    1,
                    type(uint128).max,
                    true,
                    false,
                    true,
                    block.timestamp
                );
            assertEq(shares, amount / 1 ether);
            out = value - fee;
        } else {
            (uint128 shares,,) = IHanjiProxy(PROXY)
                .placeMarketOrderWithTargetValue(
                    false,
                    uint128(amount),
                    999_999_000_000_000_000_000,
                    type(uint128).max,
                    true,
                    block.timestamp
                );
            out = uint256(shares) * 1 ether;
        }
        vm.stopPrank();
        return out;
    }

    function _fund(address token, uint256 amount) internal {
        deal(token, address(this), amount);
        IERC20(token).forceApprove(address(adapter), type(uint256).max);
    }
}
