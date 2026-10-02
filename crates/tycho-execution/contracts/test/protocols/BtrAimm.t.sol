pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";
import {
    BtrAimmExecutor,
    BtrAimmExecutor__InvalidDataLength,
    BtrAimmExecutor__NativeUnsupported
} from "@src/executors/BtrAimmExecutor.sol";
import {LeanTychoRouter, ClientFeeParams} from "@src/LeanTychoRouter.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MOCK") {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}

/// Pulls `amountIn` from the caller and mints twice that to `recipient`.
contract MockBtrPool {
    function swap_qe(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256,
        address recipient,
        uint256 deadline
    ) external returns (uint256 out) {
        require(block.timestamp <= deadline, "expired");
        IERC20(tokenIn).transferFrom(msg.sender, address(this), amountIn);
        out = amountIn * 2;
        MockToken(tokenOut).mint(recipient, out);
    }
}

interface IAC {
    function owner() external view returns (address);
    function setPerms(address who, uint256 mask, bool on) external;
}

interface ILivePool {
    function AC() external view returns (address);
    function getRiskFlags(address token) external view returns (uint16);
    function swapCoop_cd(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 basis
    ) external returns (uint256);
}

abstract contract BtrAimmBase is Test {
    ClientFeeParams internal noFee = ClientFeeParams(0, address(0), 0, 0, "");
    BtrAimmExecutor internal executor;
    LeanTychoRouter internal router;

    function _deployRouter() internal {
        executor = new BtrAimmExecutor();
        address[] memory executors = new address[](1);
        executors[0] = address(executor);
        router = new LeanTychoRouter(executors);
    }

    function _data(address tokenIn, address tokenOut, address pool)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(tokenIn, tokenOut, pool);
    }
}

contract BtrAimmExecutorTest is BtrAimmBase {
    MockToken internal a;
    MockToken internal b;
    MockToken internal c;
    MockBtrPool internal pool1;
    MockBtrPool internal pool2;
    address internal user = address(0xA11CE);
    address internal receiver = address(0xB0B);

    function setUp() public {
        _deployRouter();
        a = new MockToken();
        b = new MockToken();
        c = new MockToken();
        pool1 = new MockBtrPool();
        pool2 = new MockBtrPool();
        a.mint(user, 1000 ether);
        vm.prank(user);
        a.approve(address(router), type(uint256).max);
    }

    function _assertClean(address token, address pool) internal view {
        assertEq(IERC20(token).balanceOf(address(router)), 0);
        assertEq(IERC20(token).balanceOf(address(executor)), 0);
        assertEq(IERC20(token).allowance(address(router), pool), 0);
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType t,
            address to,
            address tIn,
            address tOut,
            bool outputToRouter
        ) = executor.getTransferData(
            _data(address(a), address(b), address(pool1))
        );
        assertEq(
            uint8(t), uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(to, address(pool1));
        assertEq(tIn, address(a));
        assertEq(tOut, address(b));
        assertFalse(outputToRouter);
    }

    function testFundsExpectedAddressIsCaller() public view {
        assertEq(
            executor.fundsExpectedAddress(
                _data(address(a), address(b), address(pool1))
            ),
            address(this)
        );
    }

    function testInvalidDataLength() public {
        vm.expectRevert(BtrAimmExecutor__InvalidDataLength.selector);
        executor.getTransferData(abi.encodePacked(address(a), address(b)));
        vm.expectRevert(BtrAimmExecutor__InvalidDataLength.selector);
        executor.getTransferData(
            abi.encodePacked(address(a), address(b), address(pool1), uint8(0))
        );
    }

    function testNativeUnsupported() public {
        vm.expectRevert(BtrAimmExecutor__NativeUnsupported.selector);
        executor.getTransferData(_data(ETH_ADDRESS, address(b), address(pool1)));
        vm.expectRevert(BtrAimmExecutor__NativeUnsupported.selector);
        executor.getTransferData(_data(address(a), ETH_ADDRESS, address(pool1)));
    }

    function testSingleSwap() public {
        vm.prank(user);
        uint256 out = router.singleSwap(
            100 ether,
            address(a),
            address(b),
            1,
            1,
            receiver,
            noFee,
            abi.encodePacked(
                address(executor), _data(address(a), address(b), address(pool1))
            )
        );

        assertEq(out, 200 ether);
        assertEq(b.balanceOf(receiver), 200 ether);
        assertEq(a.balanceOf(user), 900 ether);
        assertEq(a.balanceOf(address(pool1)), 100 ether);
        _assertClean(address(a), address(pool1));
        _assertClean(address(b), address(pool1));
    }

    /// The pool is a standing call target of the router: a second pool in the
    /// route is funded from the router's own balance, never from the user.
    function testSequentialSwap() public {
        bytes memory hop1 = abi.encodePacked(
            address(executor), _data(address(a), address(b), address(pool1))
        );
        bytes memory hop2 = abi.encodePacked(
            address(executor), _data(address(b), address(c), address(pool2))
        );
        bytes memory swaps = abi.encodePacked(
            uint16(hop1.length), hop1, uint16(hop2.length), hop2
        );

        vm.prank(user);
        uint256 out = router.sequentialSwap(
            10 ether, address(a), address(c), 1, 1, receiver, noFee, swaps
        );

        assertEq(out, 40 ether);
        assertEq(c.balanceOf(receiver), 40 ether);
        _assertClean(address(a), address(pool1));
        _assertClean(address(b), address(pool2));
        _assertClean(address(c), address(pool2));
    }

    function testSwapMoreThanUserApprovedReverts() public {
        vm.prank(user);
        a.approve(address(router), 10 ether);
        vm.prank(user);
        vm.expectRevert();
        router.singleSwap(
            100 ether,
            address(a),
            address(b),
            1,
            1,
            receiver,
            noFee,
            abi.encodePacked(
                address(executor), _data(address(a), address(b), address(pool1))
            )
        );
    }
}

/// Monad fork against the live BTR "core" pool. The pool is `SWAP_GATED` on
/// every leg while the launch is gated: a router caller needs AC lane 0x400
/// (swapper). The manifest never grants it to the router, so the live router
/// route reverts `NotAuthorized` until the gate clears; the tests grant it by
/// pranking the AC owner (read live, `AccessControl.owner()`).
contract BtrAimmForkTest is BtrAimmBase {
    address internal constant POOL = 0xbbbbbbb04f5b762A4CdD1d89E341e2537e3267e4;
    address internal constant USDC = 0x754704Bc059F8C67012fEd69BC8A327a5aafb603;
    address internal constant WETH = 0xEE8c0E9f1BFFb4Eb878d8f15f368A02a35481242;
    uint256 internal constant LANE_SWAP = 0x400;
    uint256 internal constant LANE_COOP = 0x800;
    uint16 internal constant SWAP_GATED_BIT = 1 << 10;
    uint256 internal constant AMOUNT_IN = 10e6; // 10 USDC

    address internal user = address(0xA11CE);
    address internal receiver = address(0xB0B);
    IAC internal ac;

    function setUp() public {
        vm.createSelectFork("https://rpc.monad.xyz");
        _deployRouter();
        ac = IAC(ILivePool(POOL).AC());
        deal(USDC, user, AMOUNT_IN);
        vm.prank(user);
        IERC20(USDC).approve(address(router), type(uint256).max);
    }

    function _grant(address who, uint256 lane) internal {
        vm.prank(ac.owner());
        ac.setPerms(who, lane, true);
    }

    function _route(address tokenIn, address tokenOut, uint256 amountIn)
        internal
        returns (uint256)
    {
        return router.singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            1,
            1,
            receiver,
            noFee,
            abi.encodePacked(address(executor), _data(tokenIn, tokenOut, POOL))
        );
    }

    function testGatedRouterReverts() public {
        if (ILivePool(POOL).getRiskFlags(USDC) & SWAP_GATED_BIT == 0) return;
        vm.prank(user);
        vm.expectRevert(bytes4(keccak256("NotAuthorized()")));
        _route(USDC, WETH, AMOUNT_IN);
    }

    function testSwapThroughLivePool() public {
        _grant(address(router), LANE_SWAP);
        uint256 wethBefore = IERC20(WETH).balanceOf(receiver);

        vm.prank(user);
        uint256 out = _route(USDC, WETH, AMOUNT_IN);

        assertGt(out, 0);
        assertEq(IERC20(WETH).balanceOf(receiver) - wethBefore, out);
        assertEq(IERC20(USDC).balanceOf(user), 0);
        for (uint256 i; i < 2; ++i) {
            address t = i == 0 ? USDC : WETH;
            assertEq(IERC20(t).balanceOf(address(router)), 0);
            assertEq(IERC20(t).balanceOf(address(executor)), 0);
            assertEq(IERC20(t).allowance(address(router), POOL), 0);
        }
    }

    /// CoopArb self-trade: the Tycho leg a->b crosses the coop pool at the
    /// public fee, then the arb sells b->a with `swapCoop_cd` (basis = a
    /// spent). Priced off the same marks both ways, so the round trip pays
    /// the public fee plus the (discounted) coop fee: strictly below `spent`.
    function testCoopSelfTradeLoses() public {
        _grant(address(router), LANE_SWAP);
        _grant(address(this), LANE_COOP);
        uint256 spentUsdc = AMOUNT_IN;
        address arb = address(this);
        deal(USDC, arb, spentUsdc);
        IERC20(USDC).approve(address(router), type(uint256).max);

        receiver = arb;
        uint256 got = _route(USDC, WETH, spentUsdc);
        assertGt(got, 0);

        IERC20(WETH).approve(POOL, got);
        uint256 back = ILivePool(POOL).swapCoop_cd(WETH, USDC, got, spentUsdc);

        emit log_named_uint("usdc spent", spentUsdc);
        emit log_named_uint("usdc back", back);
        assertLt(back, spentUsdc, "round trip through one pool must lose");
    }
}
