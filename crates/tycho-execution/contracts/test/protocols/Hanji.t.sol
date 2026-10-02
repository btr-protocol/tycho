pragma solidity ^0.8.26;

import "../TestUtils.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    HanjiExecutor,
    HanjiExecutor__InvalidDataLength,
    HanjiExecutor__PartialFill,
    HanjiExecutor__TokensNotInMarket,
    IHanjiProxy
} from "../../src/executors/HanjiExecutor.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {LeanTychoRouter, ClientFeeParams} from "../../src/LeanTychoRouter.sol";

contract HanjiExecutorExposed is HanjiExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (address proxy, address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }

    receive() external payable {}
}

/// Monad mainnet fork: `forge test -n monad --match-contract HanjiExecutorTest`
/// with `MONAD_RPC_URL` set.
contract HanjiExecutorTest is TestUtils {
    // MON/USDC fast-quoter proxy.
    address constant PROXY = 0x1aeD222dda944a87703c918745b11bE13f8eEf10;
    address constant WMON = 0x3bd359C1119dA7Da1D913D1C4D2B7c461115433A;
    address constant USDC = 0x754704Bc059F8C67012fEd69BC8A327a5aafb603;
    // A block where the fast quoter is quoting.
    uint256 constant FORK_BLOCK = 109038858;

    HanjiExecutorExposed executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("monad"), FORK_BLOCK);
        executor = new HanjiExecutorExposed();
    }

    function testDecodeParams() public view {
        (address proxy, address tokenIn, address tokenOut) =
            executor.decodeParams(abi.encodePacked(PROXY, WMON, USDC));
        assertEq(proxy, PROXY);
        assertEq(tokenIn, WMON);
        assertEq(tokenOut, USDC);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(HanjiExecutor__InvalidDataLength.selector);
        executor.decodeParams(abi.encodePacked(PROXY, WMON));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(abi.encodePacked(PROXY, USDC, WMON));
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, PROXY);
        assertEq(tokenIn, USDC);
        assertEq(tokenOut, WMON);
        assertTrue(outputToRouter);
    }

    function testSwapMonToUsdc() public {
        uint256 want = _proxyOut(true, 1000);
        uint256 out = _swap(WMON, USDC, 1000 ether + 1);
        // 1000 whole shares sold; the sub-share remainder stays behind.
        assertEq(IERC20(WMON).balanceOf(address(executor)), 1);
        assertEq(out, want);
    }

    function testSwapUsdcToMon() public {
        uint256 want = _proxyOut(false, 30e6);
        uint256 out = _swap(USDC, WMON, 30e6);
        // Native MON from the proxy comes back wrapped.
        assertEq(address(executor).balance, 0);
        assertEq(out, want);
    }

    function testSwapRevertsOnForeignToken() public {
        vm.expectRevert(HanjiExecutor__TokensNotInMarket.selector);
        executor.swap(1e6, abi.encodePacked(PROXY, USDC, USDC), address(this));
    }

    function testSwapRevertsBeyondBook() public {
        uint256 amountIn = 1e12 ether;
        deal(WMON, address(executor), amountIn);
        vm.prank(address(executor));
        IERC20(WMON).approve(PROXY, amountIn);
        vm.expectRevert(HanjiExecutor__PartialFill.selector);
        executor.swap(amountIn, abi.encodePacked(PROXY, WMON, USDC), address(0));
    }

    function testDecodeIntegration() public view {
        // Rust encoder output for MON/USDC (no calldata.txt entry on this
        // branch).
        (address proxy, address tokenIn, address tokenOut) = executor.decodeParams(
            hex"1aed222dda944a87703c918745b11be13f8eef103bd359c1119da7da1d913d1c4d2b7c461115433a754704bc059f8c67012fed69bc8a327a5aafb603"
        );
        assertEq(proxy, PROXY);
        assertEq(tokenIn, WMON);
        assertEq(tokenOut, USDC);
    }

    function _swap(address tokenIn, address tokenOut, uint256 amountIn)
        internal
        returns (uint256)
    {
        deal(tokenIn, address(executor), amountIn);
        vm.prank(address(executor));
        IERC20(tokenIn).approve(PROXY, amountIn);
        uint256 before = IERC20(tokenOut).balanceOf(address(executor));
        executor.swap(
            amountIn, abi.encodePacked(PROXY, tokenIn, tokenOut), address(0)
        );
        uint256 out = IERC20(tokenOut).balanceOf(address(executor)) - before;
        assertGt(out, 0);
        return out;
    }

    /// The proxy's own fill for the same order from a contract taker, in
    /// buy-token units; the state is rolled back afterwards.
    function _proxyOut(bool ask, uint256 amount)
        internal
        returns (uint256 out)
    {
        uint256 snapshot = vm.snapshotState();
        HanjiTaker taker = new HanjiTaker();
        deal(ask ? WMON : USDC, address(taker), ask ? amount * 1 ether : amount);
        out = taker.fill(ask, amount);
        vm.revertToState(snapshot);
    }
}

/// A contract taker, as the router is.
contract HanjiTaker {
    address constant PROXY = 0x1aeD222dda944a87703c918745b11bE13f8eEf10;
    address constant WMON = 0x3bd359C1119dA7Da1D913D1C4D2B7c461115433A;
    address constant USDC = 0x754704Bc059F8C67012fEd69BC8A327a5aafb603;

    receive() external payable {}

    function fill(bool ask, uint256 amount) external returns (uint256) {
        IERC20(ask ? WMON : USDC).approve(PROXY, type(uint256).max);
        if (ask) {
            (, uint128 shares, uint128 value, uint128 fee) = IHanjiProxy(PROXY)
                .placeOrder(
                    true,
                    uint128(amount),
                    1,
                    type(uint128).max,
                    true,
                    false,
                    true,
                    block.timestamp
                );
            require(shares == amount, "partial fill");
            return value - fee;
        }
        (uint128 bought,,) = IHanjiProxy(PROXY)
            .placeMarketOrderWithTargetValue(
                false,
                uint128(amount),
                999_999_000_000_000_000_000,
                type(uint128).max,
                true,
                block.timestamp
            );
        return uint256(bought) * 1 ether;
    }
}

/// Hanji through the lean router, Monad fork.
contract HanjiLeanRouterTest is TestUtils {
    address constant PROXY = 0x1aeD222dda944a87703c918745b11bE13f8eEf10;
    address constant WMON = 0x3bd359C1119dA7Da1D913D1C4D2B7c461115433A;
    address constant USDC = 0x754704Bc059F8C67012fEd69BC8A327a5aafb603;
    uint256 constant FORK_BLOCK = 109038858;

    HanjiExecutor executor;
    LeanTychoRouter router;
    address user = makeAddr("user");
    address receiver = makeAddr("receiver");

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("monad"), FORK_BLOCK);
        executor = new HanjiExecutor();
        address[] memory executors = new address[](1);
        executors[0] = address(executor);
        router = new LeanTychoRouter(executors);
    }

    function testSingleSwapSellX() public {
        // Whole shares only: nothing is left over at the router.
        uint256 out = _single(WMON, USDC, 1000 ether);
        assertGt(out, 0);
        assertEq(IERC20(USDC).balanceOf(receiver), out);
        _assertRouterClean(WMON, USDC);
    }

    function testSingleSwapSellY() public {
        uint256 out = _single(USDC, WMON, 30e6);
        assertGt(out, 0);
        assertEq(IERC20(WMON).balanceOf(receiver), out);
        // Share rounding may leave a USDC remainder; the output is wrapped
        // and forwarded, and no native or allowance is left.
        assertEq(IERC20(WMON).balanceOf(address(router)), 0);
        assertEq(address(router).balance, 0);
        assertEq(IERC20(USDC).allowance(address(router), PROXY), 0);
        assertLt(IERC20(USDC).balanceOf(address(router)), 1e6);
    }

    function testSingleSwapRevertsOnUnknownExecutor() public {
        deal(WMON, user, 1000 ether);
        vm.startPrank(user);
        IERC20(WMON).approve(address(router), 1000 ether);
        address rogue = address(new HanjiExecutor());
        vm.expectRevert(
            abi.encodeWithSelector(LeanTychoRouter.BadExecutor.selector, rogue)
        );
        router.singleSwap(
            1000 ether,
            WMON,
            USDC,
            0,
            0,
            receiver,
            ClientFeeParams(0, address(0), 0, 0, ""),
            abi.encodePacked(rogue, PROXY, WMON, USDC)
        );
        vm.stopPrank();
    }

    function _single(address tokenIn, address tokenOut, uint256 amountIn)
        internal
        returns (uint256 out)
    {
        deal(tokenIn, user, amountIn);
        vm.startPrank(user);
        IERC20(tokenIn).approve(address(router), amountIn);
        uint256 g = gasleft();
        out = router.singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            0,
            1,
            receiver,
            ClientFeeParams(0, address(0), 0, 0, ""),
            abi.encodePacked(address(executor), PROXY, tokenIn, tokenOut)
        );
        emit log_named_uint("lean router singleSwap gas", g - gasleft());
        vm.stopPrank();
        assertEq(IERC20(tokenIn).balanceOf(user), 0);
    }

    function _assertRouterClean(address tokenIn, address tokenOut)
        internal
        view
    {
        assertEq(IERC20(tokenIn).balanceOf(address(router)), 0);
        assertEq(IERC20(tokenOut).balanceOf(address(router)), 0);
        assertEq(address(router).balance, 0);
        assertEq(IERC20(tokenIn).allowance(address(router), PROXY), 0);
        assertEq(IERC20(tokenOut).allowance(address(router), PROXY), 0);
    }
}
