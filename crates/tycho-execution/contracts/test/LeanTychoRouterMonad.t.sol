// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {Test, console2} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {LeanTychoRouter, ClientFeeParams} from "@src/LeanTychoRouter.sol";

interface IOldRouter {
    function setExecutors(address[] memory) external;
}

interface IFeeCalc {
    function setPositiveSlippageEnabled(bool) external;
}

/// @title Lean vs live TychoRouterV3 on Monad, identical Fynd-shaped calldata
/// @notice MONAD_RPC_URL=https://rpc.monad.xyz forge test --network monad
///         --match-path test/LeanTychoRouterMonad.t.sol -vv. The live router
///         gets its 6 executors from the owner Safe, 24 h warped, positive
///         slippage off (the intended production config).
contract LeanTychoRouterMonadTest is Test {
    address constant OLD = 0xbbbbbbb0085A5C0BA0bd40C6Bb048685d4e73Dac;
    address constant FEE_CALC = 0xbbbbbbb00e29099124a3de9ACE5D7A64DAd8f8Fe;
    address constant SAFE = 0xb6cC92E2c63aB6bdaD6bD333892e3990F808f618;
    address constant UNIV3_EX = 0xbbbbbbb01f183CaF30f9269dA665b3c47449aC83;
    address constant UNIV4_EX = 0xBBBBBBB0331B1e4ac5F821ef928918f014598f5f;
    address constant BALV3_EX = 0xbbbbbbb0352D36e84725A91732F65Ee03D5e423E;
    address constant CURVE_EX = 0xbbbbbbb03625B037f6317301A68Bc73beD14160B;
    address constant WRAP_EX = 0xbbbbbbb03b6F50A52BCCF194E6e2f03A18462849;
    address constant KURU_EX = 0xbbbbbbb03e8Ef4278A52DC145a4408D1015bb854;

    address constant USDC = 0x754704Bc059F8C67012fEd69BC8A327a5aafb603;
    address constant WMON = 0x3bd359C1119dA7Da1D913D1C4D2B7c461115433A;
    address constant WETH = 0xEE8c0E9f1BFFb4Eb878d8f15f368A02a35481242;
    address constant CBBTC = 0xd18B7EC58Cdf4876f6AFebd3Ed1730e4Ce10414b;
    address constant UNI_WMON_USDC_500 =
        0x5Bc39A29EA5d8315263EC8939d6AD393dEbC34E9;
    address constant PCS_WMON_USDC_500 =
        0x63e48B725540A3Db24ACF6682a29f877808C53F2;
    address constant UNI_WETH_USDC_3000 =
        0x25EF1a210fF55BcEe9F8fee979aAFf6bD1bE5Bf1;
    address constant AUSD = 0x00000000eFE302BEAA2b3e6e1b18d08D69a9012a;
    address constant WN_USDC = 0x8d5c2Df3Eef09088Fcccf3376D8EcD0Dd505f642;
    address constant WN_USDT0 = 0x4e8aaecCE10ad9394e96fE5f2bd4e587A7B04298;
    address constant CURVE_AUSD_USDC_USDT0 =
        0x942644106B073E30D72c2C5D7529D5C296ea91ab;
    address constant BAL_WNUSDC_WNUSDT0 =
        0x2DAA146dfB7EAef0038F9F15B2EC1e4DE003f72b;
    address constant KURU_BOOK_BTC = 0x40c49F171202F91ff5d2faE34c22dD2BFdD22aF0;

    LeanTychoRouter lean;
    address user = makeAddr("user");
    ClientFeeParams noFee;

    function setUp() public {
        vm.createSelectFork(
            vm.envOr("MONAD_RPC_URL", string("https://rpc.monad.xyz")),
            vm.envOr("FORK_BLOCK", uint256(109_790_000))
        );
        address[] memory e = new address[](6);
        (e[0], e[1], e[2], e[3], e[4], e[5]) =
        (UNIV3_EX, UNIV4_EX, BALV3_EX, CURVE_EX, WRAP_EX, KURU_EX);
        lean = new LeanTychoRouter(e);
        vm.startPrank(SAFE);
        IOldRouter(OLD).setExecutors(e);
        IFeeCalc(FEE_CALC).setPositiveSlippageEnabled(false);
        vm.stopPrank();
        vm.warp(block.timestamp + 1 days + 1);
    }

    function _v3(address tIn, address tOut, uint24 fee, address pool)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(UNIV3_EX, tIn, tOut, fee, pool, tIn < tOut);
    }

    function _ple(bytes memory x) internal pure returns (bytes memory) {
        return abi.encodePacked(uint16(x.length), x);
    }

    /// @dev Same calldata to both routers from a fresh state; outputs must match.
    function _bench(
        string memory tag,
        address tIn,
        address tOut,
        uint256 amt,
        bytes memory call
    ) internal returns (uint256 gOld, uint256 gNew) {
        uint256 snap = vm.snapshotState();
        uint256 oOld;
        (gOld, oOld) = _run(OLD, tIn, tOut, amt, call);
        vm.revertToState(snap);
        uint256 oNew;
        (gNew, oNew) = _run(address(lean), tIn, tOut, amt, call);
        assertEq(oNew, oOld, "same output");
        assertGt(oNew, 0, "output");
        address[3] memory t = [tIn, tOut, WMON];
        for (uint256 i = 0; i < 3; ++i) {
            assertEq(IERC20(t[i]).balanceOf(address(lean)), 0, "residual");
        }
        assertEq(address(lean).balance, 0, "native residual");
        console2.log(tag);
        console2.log("  old gas", gOld);
        console2.log("  new gas", gNew);
    }

    function _run(
        address router,
        address tIn,
        address tOut,
        uint256 amt,
        bytes memory call
    ) internal returns (uint256 gas, uint256 out) {
        deal(tIn, user, amt);
        vm.startPrank(user);
        IERC20(tIn).approve(router, amt);
        uint256 b0 = IERC20(tOut).balanceOf(user);
        gas = gasleft();
        (bool ok, bytes memory r) = router.call(call);
        gas -= gasleft();
        vm.stopPrank();
        if (!ok) {
            assembly {
                revert(add(r, 32), mload(r))
            }
        }
        out = IERC20(tOut).balanceOf(user) - b0;
        assertEq(IERC20(tIn).allowance(user, router), 0, "approval used");
    }

    function _single(address tIn, address tOut, uint256 amt, bytes memory d)
        internal
        view
        returns (bytes memory)
    {
        return abi.encodeCall(
            LeanTychoRouter.singleSwap, (amt, tIn, tOut, 1, 1, user, noFee, d)
        );
    }

    function _seq(address tIn, address tOut, uint256 amt, bytes memory s)
        internal
        view
        returns (bytes memory)
    {
        return abi.encodeCall(
            LeanTychoRouter.sequentialSwap,
            (amt, tIn, tOut, 1, 1, user, noFee, s)
        );
    }

    function _split(
        address tIn,
        address tOut,
        uint256 amt,
        uint256 n,
        bytes memory s
    ) internal view returns (bytes memory) {
        return abi.encodeCall(
            LeanTychoRouter.splitSwap, (amt, tIn, tOut, 1, 1, n, user, noFee, s)
        );
    }

    function testBenchSingleV3() public {
        bytes memory d = _v3(USDC, WMON, 500, UNI_WMON_USDC_500);
        _bench(
            "single uni v3 1000 USDC->WMON",
            USDC,
            WMON,
            1000e6,
            _single(USDC, WMON, 1000e6, d)
        );
    }

    function testBenchSequentialTwoHop() public {
        bytes memory s = abi.encodePacked(
            _ple(_v3(WETH, USDC, 3000, UNI_WETH_USDC_3000)),
            _ple(_v3(USDC, WMON, 500, PCS_WMON_USDC_500))
        );
        _bench(
            "seq 0.3 WETH->USDC(uni)->WMON(pcs)",
            WETH,
            WMON,
            0.3e18,
            _seq(WETH, WMON, 0.3e18, s)
        );
    }

    function testBenchSplit() public {
        uint24 s60 = uint24(uint256(0xffffff) * 60 / 100);
        bytes memory s = abi.encodePacked(
            _ple(
                abi.encodePacked(
                    uint8(0),
                    uint8(1),
                    s60,
                    _v3(USDC, WMON, 500, UNI_WMON_USDC_500)
                )
            ),
            _ple(
                abi.encodePacked(
                    uint8(0),
                    uint8(1),
                    uint24(0),
                    _v3(USDC, WMON, 500, PCS_WMON_USDC_500)
                )
            )
        );
        _bench(
            "split 60/40 uni/pcs 2000 USDC->WMON",
            USDC,
            WMON,
            2000e6,
            _split(USDC, WMON, 2000e6, 2, s)
        );
    }

    /// @dev 3 tokens, 2 hops, a split on hop 1: USDC→{WMON uni, WMON pcs}→
    ///      USDC→WETH is not a cycle; WMON→USDC→WETH via the split leg.
    function testBenchSplitTwoHop() public {
        uint24 s50 = uint24(uint256(0xffffff) / 2);
        bytes memory s = abi.encodePacked(
            _ple(
                abi.encodePacked(
                    uint8(0),
                    uint8(1),
                    s50,
                    _v3(WMON, USDC, 500, UNI_WMON_USDC_500)
                )
            ),
            _ple(
                abi.encodePacked(
                    uint8(0),
                    uint8(1),
                    uint24(0),
                    _v3(WMON, USDC, 500, PCS_WMON_USDC_500)
                )
            ),
            _ple(
                abi.encodePacked(
                    uint8(1),
                    uint8(2),
                    uint24(0),
                    _v3(USDC, WETH, 3000, UNI_WETH_USDC_3000)
                )
            )
        );
        _bench(
            "split+hop 20000 WMON->USDC(uni|pcs)->WETH",
            WMON,
            WETH,
            20_000e18,
            _split(WMON, WETH, 20_000e18, 3, s)
        );
    }

    function testBenchKuru() public {
        bytes memory d = abi.encodePacked(KURU_EX, KURU_BOOK_BTC, USDC, CBBTC);
        _bench(
            "single kuru 1000 USDC->cbBTC",
            USDC,
            CBBTC,
            1000e6,
            _single(USDC, CBBTC, 1000e6, d)
        );
    }

    function testBenchV4() public {
        bytes memory d = abi.encodePacked(
            UNIV4_EX,
            USDC,
            WETH,
            USDC < WETH,
            false,
            WETH,
            uint24(500),
            int24(10),
            address(0),
            uint16(0)
        );
        _bench(
            "single uni v4 1000 USDC->WETH",
            USDC,
            WETH,
            1000e6,
            _single(USDC, WETH, 1000e6, d)
        );
    }

    /// @dev coins [AUSD, USDC, USDT0], pool type 1 (core StableSwap factory).
    function testBenchCurve() public {
        bytes memory d = abi.encodePacked(
            CURVE_EX,
            USDC,
            AUSD,
            CURVE_AUSD_USDC_USDT0,
            uint8(1),
            uint8(1),
            uint8(0)
        );
        _bench(
            "single curve 100 USDC->AUSD",
            USDC,
            AUSD,
            100e6,
            _single(USDC, AUSD, 100e6, d)
        );
    }

    function testBenchBalancerV3() public {
        bytes memory d =
            abi.encodePacked(BALV3_EX, WN_USDC, WN_USDT0, BAL_WNUSDC_WNUSDT0);
        _bench(
            "single balancer v3 100 wnUSDC->wnUSDT0",
            WN_USDC,
            WN_USDT0,
            100e6,
            _single(WN_USDC, WN_USDT0, 100e6, d)
        );
    }
}
