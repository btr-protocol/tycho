// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {LeanTychoRouter, ClientFeeParams} from "@src/LeanTychoRouter.sol";
import {TychoRouterV3} from "@src/TychoRouterV3.sol";

contract Tok is ERC20 {
    constructor(string memory s) ERC20(s, s) {}

    function mint(address to, uint256 a) external {
        _mint(to, a);
    }
}

/// @dev Pays `out = in * 2` of `tOut`. `mode`: 0 = input pre-transferred,
///      1 = pulls via transferFrom (ProtocolWillDebit), 2 = V3-style callback
///      to `msg.sender` before checking its balance.
contract Pool {
    IERC20 public immutable tIn;
    Tok public immutable tOut;
    uint8 public immutable mode;
    bytes public hook; // a call to make mid-swap (attack scenarios)
    address public hookTarget;
    uint256 public pulled; // ProtocolWillDebit: pull this much (0 = all)

    constructor(IERC20 i, Tok o, uint8 m) {
        (tIn, tOut, mode) = (i, o, m);
    }

    function setHook(address t, bytes calldata h) external {
        (hookTarget, hook) = (t, h);
    }

    function setPulled(uint256 p) external {
        pulled = p;
    }

    function swap(uint256 amt, address to) external {
        if (hook.length != 0) {
            (bool ok, bytes memory r) = hookTarget.call(hook);
            if (!ok) {
                assembly {
                    revert(add(r, 32), mload(r))
                }
            }
        }
        uint256 b0 = tIn.balanceOf(address(this));
        if (mode == 1) {
            tIn.transferFrom(
                msg.sender, address(this), pulled == 0 ? amt : pulled
            );
            amt = pulled == 0 ? amt : pulled;
        }
        if (mode == 2) {
            ICallbackTarget(msg.sender).fakeSwapCallback(amt);
            require(tIn.balanceOf(address(this)) >= b0 + amt, "unpaid");
        }
        tOut.mint(to, amt * 2);
    }
}

interface ICallbackTarget {
    function fakeSwapCallback(uint256) external;
}

/// @dev Executor for `Pool`; data = tokenIn(20) tokenOut(20) pool(20).
contract MockExecutor is IExecutor, ICallback {
    function _d(bytes calldata d)
        internal
        pure
        returns (address i, address o, Pool p)
    {
        i = address(bytes20(d[0:20]));
        o = address(bytes20(d[20:40]));
        p = Pool(address(bytes20(d[40:60])));
    }

    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (,, Pool p) = _d(data);
        p.swap(amountIn, receiver);
    }

    function getTransferData(bytes calldata data)
        external
        view
        returns (
            TransferManager.TransferType t,
            address r,
            address i,
            address o,
            bool toRouter
        )
    {
        Pool p;
        (i, o, p) = _d(data);
        uint8 m = p.mode();
        t = m == 0
            ? TransferManager.TransferType.Transfer
            : m == 1
                ? TransferManager.TransferType.ProtocolWillDebit
                : m == 2
                    ? TransferManager.TransferType.None
                    : TransferManager.TransferType.TransferNativeInExecutor;
        r = address(p);
        toRouter = false;
    }

    function fundsExpectedAddress(bytes calldata data)
        external
        view
        returns (address)
    {
        (,, Pool p) = _d(data);
        return p.mode() == 0 ? address(p) : msg.sender;
    }

    function handleCallback(bytes calldata)
        external
        pure
        returns (bytes memory)
    {
        return abi.encode(bytes(""));
    }

    function getCallbackTransferData(bytes calldata, address, address caller)
        external
        pure
        returns (TransferManager.TransferType, address)
    {
        return (TransferManager.TransferType.Transfer, caller);
    }
}

contract LeanTychoRouterTest is Test {
    LeanTychoRouter router;
    MockExecutor ex;
    Tok a;
    Tok b;
    Tok c;
    address user = makeAddr("user");
    address victim = makeAddr("victim");
    ClientFeeParams noFee;

    function setUp() public {
        ex = new MockExecutor();
        address[] memory e = new address[](1);
        e[0] = address(ex);
        router = new LeanTychoRouter(e);
        a = new Tok("A");
        b = new Tok("B");
        c = new Tok("C");
        a.mint(user, 100e18);
        vm.prank(user);
        a.approve(address(router), type(uint256).max);
    }

    function _pool(Tok i, Tok o, uint8 m) internal returns (Pool) {
        return new Pool(i, o, m);
    }

    function _hop(Pool p) internal view returns (bytes memory) {
        return abi.encodePacked(
            address(ex), address(p.tIn()), address(p.tOut()), address(p)
        );
    }

    function _ple(bytes memory x) internal pure returns (bytes memory) {
        return abi.encodePacked(uint16(x.length), x);
    }

    function _split(uint8 i, uint8 o, uint24 s, Pool p)
        internal
        view
        returns (bytes memory)
    {
        return _ple(abi.encodePacked(i, o, s, _hop(p)));
    }

    function _single(Pool p, uint256 amt, uint256 minOut)
        internal
        returns (uint256)
    {
        return _go(_hop(p), address(p.tIn()), address(p.tOut()), amt, minOut);
    }

    /// @dev Only the prank and the router call: safe after `expectRevert`.
    function _go(
        bytes memory d,
        address i,
        address o,
        uint256 amt,
        uint256 minOut
    ) internal returns (uint256) {
        vm.prank(user);
        return router.singleSwap(amt, i, o, minOut, minOut, user, noFee, d);
    }

    function _clean(Tok[3] memory ts, address[3] memory spenders)
        internal
        view
    {
        for (uint256 i = 0; i < 3; ++i) {
            assertEq(ts[i].balanceOf(address(router)), 0, "router balance");
            assertEq(ts[i].balanceOf(address(ex)), 0, "executor balance");
            for (uint256 j = 0; j < 3; ++j) {
                assertEq(
                    ts[i].allowance(address(router), spenders[j]),
                    0,
                    "allowance"
                );
            }
        }
    }

    function testSingleTransfer() public {
        Pool p = _pool(a, b, 0);
        assertEq(_single(p, 1e18, 2e18), 2e18);
        assertEq(b.balanceOf(user), 2e18);
        assertEq(a.balanceOf(address(p)), 1e18);
        _clean([a, b, c], [address(p), address(0), address(0)]);
    }

    function testSingleDebitAndCallback() public {
        Pool p1 = _pool(a, b, 1);
        Pool p2 = _pool(a, b, 2);
        assertEq(_single(p1, 1e18, 1), 2e18);
        assertEq(_single(p2, 1e18, 1), 2e18);
        assertEq(b.balanceOf(user), 4e18);
        _clean([a, b, c], [address(p1), address(p2), address(0)]);
    }

    function testMinOut() public {
        Pool p = _pool(a, b, 0);
        bytes memory d = _hop(p);
        vm.expectRevert(
            abi.encodeWithSelector(
                LeanTychoRouter.NegativeSlippage.selector, 2e18, 2e18 + 1
            )
        );
        _go(d, address(a), address(b), 1e18, 2e18 + 1);
    }

    /// @dev a→b pre-transferred (prefunded by hop 1), b→c via callback.
    function testSequentialTwoHop() public {
        Pool p1 = _pool(a, b, 0);
        Pool p2 = _pool(b, c, 0);
        Pool p3 = _pool(b, c, 2);
        bytes memory s1 = abi.encodePacked(_ple(_hop(p1)), _ple(_hop(p2)));
        bytes memory s2 = abi.encodePacked(_ple(_hop(p1)), _ple(_hop(p3)));
        vm.startPrank(user);
        uint256 o1 = router.sequentialSwap(
            1e18, address(a), address(c), 1, 1, user, noFee, s1
        );
        uint256 o2 = router.sequentialSwap(
            1e18, address(a), address(c), 1, 1, user, noFee, s2
        );
        vm.stopPrank();
        assertEq(o1, 4e18);
        assertEq(o2, 4e18);
        assertEq(c.balanceOf(user), 8e18);
        _clean([a, b, c], [address(p1), address(p2), address(p3)]);
    }

    /// @dev 30% a→b (transfer) + rest a→b (callback), then all b→c (debit).
    function testSplitTwoHop() public {
        Pool p1 = _pool(a, b, 0);
        Pool p2 = _pool(a, b, 2);
        Pool p3 = _pool(b, c, 1);
        uint24 s30 = uint24(uint256(0xffffff) * 30 / 100);
        bytes memory s = abi.encodePacked(
            _split(0, 1, s30, p1), _split(0, 1, 0, p2), _split(1, 2, 0, p3)
        );
        vm.prank(user);
        uint256 o = router.splitSwap(
            1e18, address(a), address(c), 1, 1, 3, user, noFee, s
        );
        assertEq(o, 4e18);
        assertEq(c.balanceOf(user), 4e18);
        assertEq(a.balanceOf(user), 99e18, "exactly amountIn pulled");
        _clean([a, b, c], [address(p1), address(p2), address(p3)]);
    }

    function testSplitOverBudgetReverts() public {
        Pool p1 = _pool(a, b, 0);
        uint24 s60 = uint24(uint256(0xffffff) * 60 / 100);
        bytes memory s =
            abi.encodePacked(_split(0, 1, s60, p1), _split(0, 1, s60, p1));
        vm.prank(user);
        vm.expectPartialRevert(LeanTychoRouter.ExceedsAmountIn.selector);
        router.splitSwap(1e18, address(a), address(b), 1, 1, 2, user, noFee, s);
    }

    function testUnknownExecutorReverts() public {
        Pool p = _pool(a, b, 0);
        MockExecutor rogue = new MockExecutor();
        bytes memory d = abi.encodePacked(
            address(rogue), address(a), address(b), address(p)
        );
        vm.prank(user);
        vm.expectRevert(
            abi.encodeWithSelector(
                LeanTychoRouter.BadExecutor.selector, address(rogue)
            )
        );
        router.singleSwap(1e18, address(a), address(b), 1, 1, user, noFee, d);
    }

    function testCallbackOutsideSwapReverts() public {
        vm.expectRevert(LeanTychoRouter.NoCallback.selector);
        ICallbackTarget(address(router)).fakeSwapCallback(1);
    }

    /// @dev A contract in the route calls back first: it is paid from the
    ///      caller's budget, the real pool's callback finds the slot spent.
    function testCallbackOneShot() public {
        Pool p = _pool(a, b, 2);
        Thief t = new Thief();
        p.setHook(
            address(t),
            abi.encodeCall(
                Thief.call,
                (
                    address(router),
                    abi.encodeCall(ICallbackTarget.fakeSwapCallback, (0))
                )
            )
        );
        bytes memory d = _hop(p);
        vm.expectRevert(LeanTychoRouter.NoCallback.selector);
        _go(d, address(a), address(b), 1e18, 1);
        assertEq(a.balanceOf(user), 100e18);
    }

    function testReentryLocked() public {
        Pool p = _pool(a, b, 0);
        p.setHook(
            address(router),
            abi.encodeCall(
                LeanTychoRouter.singleSwap,
                (1, address(a), address(b), 1, 1, user, noFee, _hop(p))
            )
        );
        bytes memory d = _hop(p);
        vm.expectRevert(LeanTychoRouter.Locked.selector);
        _go(d, address(a), address(b), 1e18, 1);
    }

    /// @dev `victim` approved the router; nobody else can spend it.
    function testCannotSpendThirdPartyApproval() public {
        a.mint(victim, 10e18);
        vm.prank(victim);
        a.approve(address(router), type(uint256).max);
        address attacker = makeAddr("attacker");
        vm.prank(attacker);
        a.approve(address(router), type(uint256).max);
        bytes memory e = abi.encodeWithSignature(
            "ERC20InsufficientBalance(address,uint256,uint256)",
            attacker,
            0,
            1e18
        );
        // direct-transfer pool, then a callback-paid pool: both pull from
        // the caller only.
        for (uint8 m = 0; m < 3; m += 2) {
            bytes memory d = _hop(_pool(a, b, m));
            vm.prank(attacker);
            vm.expectRevert(e);
            router.singleSwap(
                1e18, address(a), address(b), 1, 1, attacker, noFee, d
            );
        }
        assertEq(a.balanceOf(victim), 10e18);
    }

    function testWrongTokenPullReverts() public {
        Pool p = _pool(b, c, 0);
        b.mint(user, 1e18);
        bytes memory d = _hop(p);
        vm.prank(user);
        vm.expectRevert(
            abi.encodeWithSelector(
                LeanTychoRouter.WrongTokenIn.selector, address(b)
            )
        );
        router.singleSwap(1e18, address(a), address(c), 1, 1, user, noFee, d);
    }

    function testUnconsumedApprovalRevoked() public {
        Pool p = _pool(a, b, 1);
        p.setPulled(0.5e18);
        assertEq(_single(p, 1e18, 1), 1e18);
        assertEq(a.allowance(address(router), address(p)), 0);
        // the unpulled half stays at the router: the caller's loss, by design
        assertEq(a.balanceOf(address(router)), 0.5e18);
    }

    function testConstructorRejects() public {
        address[] memory e = new address[](13);
        vm.expectRevert(LeanTychoRouter.ExecutorCount.selector);
        new LeanTychoRouter(e);
        e = new address[](2);
        (e[0], e[1]) = (address(ex), address(ex));
        vm.expectRevert(
            abi.encodeWithSelector(
                LeanTychoRouter.BadExecutor.selector, address(ex)
            )
        );
        new LeanTychoRouter(e);
        e[1] = user;
        vm.expectRevert(
            abi.encodeWithSelector(LeanTychoRouter.BadExecutor.selector, user)
        );
        new LeanTychoRouter(e);
        assertEq(router.executors()[0], address(ex));
        assertEq(router.executors()[1], address(0));
    }

    function testZeroExecutorRejected() public {
        bytes memory d = abi.encodePacked(address(0), address(a), address(b));
        vm.prank(user);
        vm.expectRevert(
            abi.encodeWithSelector(
                LeanTychoRouter.BadExecutor.selector, address(0)
            )
        );
        router.singleSwap(1e18, address(a), address(b), 1, 1, user, noFee, d);
    }

    function testStrayNativeFromEoaRefused() public {
        vm.deal(user, 1 ether);
        vm.prank(user);
        (bool ok,) = address(router).call{value: 1}("");
        assertFalse(ok);
    }

    /// @dev Fynd/Tycho encoder output must hit the same selectors.
    function testSelectorsMatchTychoRouterV3() public pure {
        assertEq(
            LeanTychoRouter.singleSwap.selector,
            TychoRouterV3.singleSwap.selector
        );
        assertEq(
            LeanTychoRouter.sequentialSwap.selector,
            TychoRouterV3.sequentialSwap.selector
        );
        assertEq(
            LeanTychoRouter.splitSwap.selector, TychoRouterV3.splitSwap.selector
        );
    }

    function testEmptyExecutorSetRejected() public {
        vm.expectRevert(LeanTychoRouter.ExecutorCount.selector);
        new LeanTychoRouter(new address[](0));
    }

    /// @dev A first hop that expects native from the router pulls nothing:
    ///      refused, so stray router balance is never the input.
    function testNativeInFirstHopRejected() public {
        Pool p = _pool(a, b, 3);
        bytes memory d = _hop(p);
        vm.expectRevert(
            abi.encodeWithSelector(
                LeanTychoRouter.WrongTokenIn.selector, address(a)
            )
        );
        _go(d, address(a), address(b), 1e18, 1);
    }

    function testSplitBadRouteRejected() public {
        vm.startPrank(user);
        vm.expectRevert(LeanTychoRouter.BadRoute.selector);
        router.splitSwap(1, address(a), address(a), 1, 1, 2, user, noFee, "");
        vm.expectRevert(LeanTychoRouter.BadRoute.selector);
        router.splitSwap(1, address(a), address(b), 1, 1, 1, user, noFee, "");
        vm.stopPrank();
    }
}

contract Thief {
    function call(address t, bytes calldata d) external {
        (bool ok,) = t.call(d);
        require(ok, "thief");
    }
}
