// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {PammBase, console2} from "./PammBase.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {LeanTychoRouter, ClientFeeParams} from "@src/LeanTychoRouter.sol";
import {ThogExecutor} from "@src/executors/ThogExecutor.sol";
import {WooFiExecutor} from "@src/executors/WooFiExecutor.sol";
import {PoeExecutor} from "@src/executors/PoeExecutor.sol";
import {HanjiExecutor} from "@src/executors/HanjiExecutor.sol";
import {CrystalExecutor} from "@src/executors/CrystalExecutor.sol";
import {CapricornExecutor} from "@src/executors/CapricornExecutor.sol";

interface IThogSwap {
    function makerSwapExactInput(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address recipient,
        uint256 deadlineBlock
    ) external returns (uint256);
}

interface IWooSwap {
    function swap(
        address fromToken,
        address toToken,
        uint256 fromAmount,
        uint256 minToAmount,
        address to,
        address rebateTo
    ) external returns (uint256);
}

interface IPoeSwap {
    function swap(address recipient, bool xToY, uint256 amountIn, bytes calldata)
        external
        returns (int256, int256);
}

interface ICrystalSwap {
    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        uint256 deadline,
        address referrer
    ) external returns (uint256[] memory);
}

interface ICapricornSwap {
    function swap(
        bool zeroForOne,
        int256 amountSpecified,
        uint256 amountLimit,
        address recipient,
        bytes calldata endorsement,
        bytes calldata data
    ) external returns (int256, int256);
}

/// Stands in for CoopArb: holds the input, runs one leg three ways.
/// direct  = the venue's own entry, inline (the floor);
/// adapter = fund per the executor's transfer type, one delegatecall, no router;
/// router  = approve LeanTychoRouter, `singleSwap` (today's TYCHO leg).
contract Host {
    // transient: a real host keeps per-leg context out of storage
    address transient exec;
    address transient tIn;
    uint256 transient amt;
    bool transient poeRet;

    // ---- direct ---------------------------------------------------------
    function thog(address amm, address a, address b, uint256 x) external {
        IERC20(a).approve(amm, x);
        IThogSwap(amm).makerSwapExactInput(a, b, x, 0, address(this), block.number);
    }

    function woo(address pool, address a, address b, uint256 x) external {
        IERC20(a).transfer(pool, x);
        IWooSwap(pool).swap(a, b, x, 0, address(this), address(this));
    }

    function poe(address pool, address a, bool xy, uint256 x) external {
        (tIn, amt, poeRet) = (a, x, true);
        IPoeSwap(pool).swap(address(this), xy, x, "");
    }

    function crystal(address c, address a, address b, uint256 x) external {
        IERC20(a).approve(c, x);
        address[] memory p = new address[](2);
        (p[0], p[1]) = (a, b);
        ICrystalSwap(c).swapExactTokensForTokens(x, 0, p, address(this), block.timestamp, address(0));
    }

    function capricorn(address pool, address a, bool z, uint256 x) external {
        (tIn, amt) = (a, x);
        ICapricornSwap(pool).swap(z, int256(x), 1, address(this), "", "");
    }

    // ---- adapter (delegatecall, no router) ------------------------------
    function adapter(address e, bytes calldata data, uint256 x) external {
        (TransferManager.TransferType t, address to, address a,,) =
            IExecutor(e).getTransferData(data);
        if (t == TransferManager.TransferType.Transfer) IERC20(a).transfer(to, x);
        else if (t == TransferManager.TransferType.ProtocolWillDebit) IERC20(a).approve(to, x);
        (exec, tIn, amt) = (e, a, x);
        (bool ok, bytes memory r) =
            e.delegatecall(abi.encodeCall(IExecutor.swap, (x, data, address(this))));
        if (!ok) assembly { revert(add(r, 32), mload(r)) }
        exec = address(0);
        if (t == TransferManager.TransferType.ProtocolWillDebit && IERC20(a).allowance(address(this), to) != 0) {
            IERC20(a).approve(to, 0);
        }
    }

    // ---- router ---------------------------------------------------------
    function viaRouter(LeanTychoRouter r, address a, address b, uint256 x, bytes calldata sd) external {
        IERC20(a).approve(address(r), x);
        ClientFeeParams memory f;
        r.singleSwap(x, a, b, 1, 1, address(this), f, sd);
    }

    /// Venue callbacks: direct mode pays the positive delta; adapter mode
    /// mirrors the router (pay per the executor, delegatecall handleCallback).
    fallback(bytes calldata d) external returns (bytes memory) {
        address e = exec;
        if (e == address(0)) {
            IERC20(tIn).transfer(msg.sender, amt);
            return poeRet ? abi.encodePacked(bytes4(0xfa483e72), bytes28(0)) : bytes("");
        }
        exec = address(0);
        (, address to) = ICallback(e).getCallbackTransferData(d, tIn, msg.sender);
        IERC20(tIn).transfer(to, amt);
        (bool ok, bytes memory r) = e.delegatecall(abi.encodeCall(ICallback.handleCallback, (d)));
        if (!ok) assembly { revert(add(r, 32), mload(r)) }
        return abi.decode(r, (bytes));
    }
}

/// Per venue, one fresh fork per number (same cold state): direct / adapter / router.
/// MONAD_RPC_URL=... FORK_BLOCK=... forge test --use solar --match-contract PammGasLayers -vv
contract PammGasLayers is PammBase {
    address constant THOG = 0x80c74517BCC2D67fFE02D3ED886796272F647210;
    address constant WOO = 0x5520385bFcf07Ec87C4c53A7d8d65595Dff69FA4;
    address constant POE = 0x02A8A16613a421EabaD6861fF6d8159f6D5EDB8f;
    address constant CRYSTAL = 0x508254c838B2e936B0631440c5C6E3AB3a4a98BD;
    address constant CAPRICORN = 0x63093325c05CD32b18034D3Ea29199fB7098E4df;
    address constant HANJI = 0x1aeD222dda944a87703c918745b11bE13f8eEf10;
    uint256 constant X = 100e18; // WMON in, USDC out, every venue

    Host internal h;
    address internal ex;

    function setUp() public {
        _fork();
        h = new Host();
        deal(WMON, address(h), X);
    }

    function _deploy(uint256 v) internal returns (bytes memory d) {
        if (v == 0) (ex, d) = (address(new ThogExecutor()), abi.encodePacked(THOG, WMON, USDC));
        if (v == 1) (ex, d) = (address(new WooFiExecutor()), abi.encodePacked(WOO, WMON, USDC));
        if (v == 2) (ex, d) = (address(new PoeExecutor()), abi.encodePacked(POE, WMON, USDC, true));
        if (v == 3) (ex, d) = (address(new CrystalExecutor()), abi.encodePacked(WMON, USDC));
        if (v == 4) (ex, d) = (address(new CapricornExecutor()), abi.encodePacked(CAPRICORN, WMON, USDC, false));
        if (v == 5) (ex, d) = (address(new HanjiExecutor()), abi.encodePacked(HANJI, WMON, USDC));
    }

    function _name(uint256 v) internal pure returns (string memory) {
        return ["thog", "woofi", "poe", "crystal", "capricorn", "hanji"][v];
    }

    function _out() internal view returns (uint256) {
        return IERC20(USDC).balanceOf(address(h));
    }

    function _direct(uint256 v) internal {
        uint256 g = gasleft();
        if (v == 0) h.thog(THOG, WMON, USDC, X);
        if (v == 1) h.woo(WOO, WMON, USDC, X);
        if (v == 2) h.poe(POE, WMON, true, X);
        if (v == 3) h.crystal(CRYSTAL, WMON, USDC, X);
        if (v == 4) h.capricorn(CAPRICORN, WMON, false, X);
        g -= gasleft();
        console2.log(string.concat(_name(v), " direct gas / out"), g, _out());
    }

    function _adapter(uint256 v) internal {
        bytes memory d = _deploy(v);
        uint256 g = gasleft();
        h.adapter(ex, d, X);
        g -= gasleft();
        console2.log(string.concat(_name(v), " adapter gas / out"), g, _out());
    }

    function _router(uint256 v) internal {
        bytes memory d = _deploy(v);
        _router(ex);
        uint256 g = gasleft();
        h.viaRouter(router, WMON, USDC, X, abi.encodePacked(ex, d));
        g -= gasleft();
        console2.log(string.concat(_name(v), " router gas / out"), g, _out());
    }

    function testThogDirect() public { _direct(0); }
    function testThogAdapter() public { _adapter(0); }
    function testThogRouter() public { _router(0); }
    function testWooDirect() public { _direct(1); }
    function testWooAdapter() public { _adapter(1); }
    function testWooRouter() public { _router(1); }
    function testPoeDirect() public { _direct(2); }
    function testPoeAdapter() public { _adapter(2); }
    function testPoeRouter() public { _router(2); }
    function testCrystalDirect() public { _direct(3); }
    function testCrystalAdapter() public { _adapter(3); }
    function testCrystalRouter() public { _router(3); }
    function testCapricornDirect() public { _direct(4); }
    function testCapricornAdapter() public { _adapter(4); }
    function testCapricornRouter() public { _router(4); }
    function testHanjiAdapter() public { _adapter(5); }
    function testHanjiRouter() public { _router(5); }

    /// Runtime code for the on-chain `eth_estimateGas` / `eth_createAccessList`
    /// replay with state overrides (Monad's own gas schedule): executor v at
    /// 0xe0..e5, a router over those six, Host. Writes test/gas-code.json.
    function testDumpCode() public {
        address[] memory e = new address[](6);
        for (uint256 v; v < 6; ++v) {
            _deploy(v);
            e[v] = address(uint160(0xe0 + v));
            vm.etch(e[v], ex.code);
        }
        string memory j = "c";
        for (uint256 v; v < 6; ++v) vm.serializeBytes(j, _name(v), e[v].code);
        vm.serializeBytes(j, "router", address(new LeanTychoRouter(e)).code);
        vm.writeJson(vm.serializeBytes(j, "host", address(h).code), "./test/gas-code.json");
    }
}
