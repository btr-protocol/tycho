// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "./TransferManager.sol";
import {LibSwap} from "../lib/LibSwap.sol";
import {
    LibPrefixLengthEncodedByteArray
} from "../lib/bytes/LibPrefixLengthEncodedByteArray.sol";
import {ETH_ADDRESS} from "../lib/NativeETH.sol";
import {
    IERC20,
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";

/// @dev ABI twin of TychoRouterV3's struct; this router charges no fee and
///      ignores it, so encoder calldata keeps its selectors.
struct ClientFeeParams {
    uint32 clientFeeBps;
    address clientFeeReceiver;
    uint256 maxClientContribution;
    uint256 deadline;
    bytes clientSignature;
}

/// @title LeanTychoRouter
/// @notice TychoRouterV3's `singleSwap` / `sequentialSwap` / `splitSwap`
///         (transfer-from flavour, same selectors and swap encoding) with the
///         executor set fixed at construction. No roles, no fees, no vault,
///         no permit2, no pause, no storage: nothing to administer.
/// @dev Pulls only from `msg.sender`, at most `amountIn` of `tokenIn` per
///      call. Executors are delegatecalled and must be one of the ≤ 12
///      immutables. A venue callback is accepted only while a hop is
///      running and once per hop. Holds no balance between calls: a stray
///      balance (donation, venue dust) is spendable by any later route, so it
///      is lost, never a caller's. ERC-20 in, no native `tokenIn`, no cyclic
///      split, no fee-on-transfer tokens (hops are paid the nominal amount).
contract LeanTychoRouter {
    using SafeERC20 for IERC20;
    using LibSwap for bytes;
    using LibPrefixLengthEncodedByteArray for bytes;

    error Locked();
    error ExecutorCount();
    error BadExecutor(address executor);
    error NoCallback();
    error SingleHopCycle(address token);
    error BadRoute();
    error ExceedsAmountIn(uint256 allowed, uint256 amount);
    error WrongTokenIn(address token);
    error NegativeSlippage(uint256 amountOut, uint256 minAmountOut);
    error SwapReverted(address executor);

    address internal immutable E0;
    address internal immutable E1;
    address internal immutable E2;
    address internal immutable E3;
    address internal immutable E4;
    address internal immutable E5;
    address internal immutable E6;
    address internal immutable E7;
    address internal immutable E8;
    address internal immutable E9;
    address internal immutable E10;
    address internal immutable E11;

    /// @dev Transient, keccak256("btr.leanRouter") + 0..8: lock, payer,
    ///      tokenIn, pull budget, and the running hop (executor, first,
    ///      split, amount, hop tokenIn).
    uint256 private constant _LOCK =
        0xd6e1262df546ab61185a8035ce82c9dffd43de56287d0061aade7799fe304b23;
    uint256 private constant _PAYER =
        0xd6e1262df546ab61185a8035ce82c9dffd43de56287d0061aade7799fe304b24;
    uint256 private constant _TOKEN_IN =
        0xd6e1262df546ab61185a8035ce82c9dffd43de56287d0061aade7799fe304b25;
    uint256 private constant _BUDGET =
        0xd6e1262df546ab61185a8035ce82c9dffd43de56287d0061aade7799fe304b26;
    uint256 private constant _EXEC =
        0xd6e1262df546ab61185a8035ce82c9dffd43de56287d0061aade7799fe304b27;
    uint256 private constant _FIRST =
        0xd6e1262df546ab61185a8035ce82c9dffd43de56287d0061aade7799fe304b28;
    uint256 private constant _SPLIT =
        0xd6e1262df546ab61185a8035ce82c9dffd43de56287d0061aade7799fe304b29;
    uint256 private constant _AMOUNT =
        0xd6e1262df546ab61185a8035ce82c9dffd43de56287d0061aade7799fe304b2a;
    uint256 private constant _HOP_IN =
        0xd6e1262df546ab61185a8035ce82c9dffd43de56287d0061aade7799fe304b2b;

    constructor(address[] memory executors_) {
        uint256 n = executors_.length;
        if (n == 0 || n > 12) revert ExecutorCount();
        address[12] memory e;
        for (uint256 i = 0; i < n; ++i) {
            address x = executors_[i];
            if (x.code.length == 0) revert BadExecutor(x);
            for (uint256 j = 0; j < i; ++j) {
                if (e[j] == x) revert BadExecutor(x);
            }
            e[i] = x;
        }
        (E0, E1, E2, E3, E4, E5) = (e[0], e[1], e[2], e[3], e[4], e[5]);
        (E6, E7, E8, E9, E10, E11) = (e[6], e[7], e[8], e[9], e[10], e[11]);
    }

    function executors() external view returns (address[12] memory) {
        return [E0, E1, E2, E3, E4, E5, E6, E7, E8, E9, E10, E11];
    }

    function singleSwap(
        uint256 amountIn,
        address tokenIn,
        address,
        uint256,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata,
        bytes calldata swapData
    ) external returns (uint256 amountOut) {
        _enter(tokenIn, amountIn);
        (address executor, bytes calldata data) = swapData.decodeSingleSwap();
        amountOut = _hop(executor, amountIn, data, true, false, receiver);
        _exit(amountOut, minAmountOut);
    }

    function sequentialSwap(
        uint256 amountIn,
        address tokenIn,
        address,
        uint256,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata,
        bytes calldata swaps
    ) external returns (uint256 amountOut) {
        _enter(tokenIn, amountIn);
        amountOut = amountIn;
        bool first = true;
        while (swaps.length != 0) {
            bytes calldata cur;
            (cur, swaps) = swaps.next();
            (address executor, bytes calldata data) = cur.decodeSequentialSwap();
            address to = receiver;
            if (swaps.length != 0) {
                (bytes calldata nxt,) = swaps.next();
                (address nextExec, bytes calldata nextData) =
                    nxt.decodeSequentialSwap();
                _check(nextExec);
                to = IExecutor(nextExec).fundsExpectedAddress(nextData);
            }
            amountOut = _hop(executor, amountOut, data, first, false, to);
            first = false;
        }
        _exit(amountOut, minAmountOut);
    }

    /// @dev Swap `i` moves `split`/0xffffff of token `in`'s total (0 = the
    ///      rest) to token `out`; index 0 = `tokenIn`, `nTokens - 1` =
    ///      `tokenOut`, paid straight to `receiver`.
    function splitSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        ClientFeeParams calldata,
        bytes calldata swaps
    ) external returns (uint256 amountOut) {
        if (tokenIn == tokenOut || nTokens < 2) {
            revert BadRoute();
        }
        _enter(tokenIn, amountIn);
        uint256[] memory total = new uint256[](nTokens);
        uint256[] memory left = new uint256[](nTokens);
        total[0] = amountIn;
        left[0] = amountIn;
        uint256 last = nTokens - 1;
        while (swaps.length != 0) {
            bytes calldata cur;
            (cur, swaps) = swaps.next();
            (
                uint8 i,
                uint8 o,
                uint24 split,
                address executor,
                bytes calldata data
            ) = cur.decodeSplitSwap();
            uint256 amt = split != 0 ? total[i] * split / 0xffffff : left[i];
            uint256 got = _hop(
                executor,
                amt,
                data,
                i == 0,
                true,
                o == last ? receiver : address(this)
            );
            total[o] += got;
            left[o] += got;
            left[i] -= amt;
        }
        amountOut = total[last];
        _exit(amountOut, minAmountOut);
    }

    /// @dev Venue callback (V3 `*SwapCallback`, V4 `unlockCallback`, Balancer
    ///      V3 hook): pays the running hop's input, then lets its executor
    ///      finish. One-shot: the hop slot clears before anything runs.
    fallback(bytes calldata data) external returns (bytes memory) {
        address executor;
        bool first;
        bool split;
        uint256 amount;
        address tokenIn;
        assembly ("memory-safe") {
            executor := tload(_EXEC)
            first := tload(_FIRST)
            split := tload(_SPLIT)
            amount := tload(_AMOUNT)
            tokenIn := tload(_HOP_IN)
            tstore(_EXEC, 0)
        }
        if (executor == address(0)) revert NoCallback();
        (TransferManager.TransferType t, address to) = ICallback(executor)
            .getCallbackTransferData(data, tokenIn, msg.sender);
        _pay(to, t, tokenIn, amount, first, split, true);
        bytes memory res = _delegate(
            executor, abi.encodeCall(ICallback.handleCallback, (data))
        );
        if (t == TransferManager.TransferType.ProtocolWillDebit) {
            _revoke(tokenIn, to);
        }
        return abi.decode(res, (bytes));
    }

    /// @dev Native from unwraps and venues only.
    receive() external payable {
        require(msg.sender.code.length != 0);
    }

    function _enter(address tokenIn, uint256 amountIn) private {
        bool locked;
        assembly ("memory-safe") {
            locked := tload(_LOCK)
        }
        if (locked) revert Locked();
        assembly ("memory-safe") {
            tstore(_LOCK, 1)
            tstore(_PAYER, caller())
            tstore(_TOKEN_IN, tokenIn)
            tstore(_BUDGET, amountIn)
        }
    }

    function _exit(uint256 amountOut, uint256 minAmountOut) private {
        if (amountOut < minAmountOut) {
            revert NegativeSlippage(amountOut, minAmountOut);
        }
        assembly ("memory-safe") {
            tstore(_LOCK, 0)
            tstore(_PAYER, 0)
            tstore(_TOKEN_IN, 0)
            tstore(_BUDGET, 0)
        }
    }

    /// @dev One hop: fund it per the executor's transfer type, delegatecall
    ///      `swap`, return `tokenOut` received (balance delta at the measure
    ///      point), forwarded to `receiver` if it landed here.
    function _hop(
        address executor,
        uint256 amount,
        bytes calldata data,
        bool first,
        bool split,
        address receiver
    ) private returns (uint256 out) {
        _check(executor);
        (
            TransferManager.TransferType t,
            address payTo,
            address tIn,
            address tOut,
            bool toRouter
        ) = IExecutor(executor).getTransferData(data);
        if (tIn == tOut) revert SingleHopCycle(tIn);
        assembly ("memory-safe") {
            tstore(_EXEC, executor)
            tstore(_FIRST, first)
            tstore(_SPLIT, split)
            tstore(_AMOUNT, amount)
            tstore(_HOP_IN, tIn)
        }
        address at = toRouter ? address(this) : receiver;
        uint256 b0 = _balanceOf(tOut, at);
        amount = _pay(payTo, t, tIn, amount, first, split, false);
        _delegate(
            executor, abi.encodeCall(IExecutor.swap, (amount, data, receiver))
        );
        assembly ("memory-safe") {
            tstore(_EXEC, 0)
        }
        if (t == TransferManager.TransferType.ProtocolWillDebit) {
            _revoke(tIn, payTo);
        }
        out = _balanceOf(tOut, at) - b0;
        if (toRouter && receiver != address(this)) _send(tOut, receiver, out);
    }

    /// @dev TychoRouterV3's TransferManager scenarios without vault/permit2:
    ///      first hop pulls from the payer, later hops pay from this
    ///      contract, a sequential non-callback `Transfer` is pre-funded by
    ///      the previous hop.
    function _pay(
        address to,
        TransferManager.TransferType t,
        address token,
        uint256 amount,
        bool first,
        bool split,
        bool inCallback
    ) private returns (uint256) {
        if (t == TransferManager.TransferType.ProtocolWillDebit) {
            if (first) _pull(token, address(this), amount);
            if (to != address(this)) IERC20(token).forceApprove(to, amount);
        } else if (t == TransferManager.TransferType.Transfer) {
            if (first) _pull(token, to, amount);
            else if (split || inCallback) _send(token, to, amount);
        } else if (first && t != TransferManager.TransferType.None) {
            revert WrongTokenIn(token); // native in: nothing to pull
        }
        return amount;
    }

    function _pull(address token, address to, uint256 amount) private {
        address payer;
        address tokenIn;
        uint256 budget;
        assembly ("memory-safe") {
            payer := tload(_PAYER)
            tokenIn := tload(_TOKEN_IN)
            budget := tload(_BUDGET)
        }
        if (token != tokenIn) revert WrongTokenIn(token);
        if (amount > budget) revert ExceedsAmountIn(budget, amount);
        assembly ("memory-safe") {
            tstore(_BUDGET, sub(budget, amount))
        }
        IERC20(token).safeTransferFrom(payer, to, amount);
    }

    function _send(address token, address to, uint256 amount) private {
        if (token == ETH_ADDRESS) Address.sendValue(payable(to), amount);
        else IERC20(token).safeTransfer(to, amount);
    }

    function _revoke(address token, address spender) private {
        if (IERC20(token).allowance(address(this), spender) != 0) {
            IERC20(token).forceApprove(spender, 0);
        }
    }

    function _balanceOf(address token, address who)
        private
        view
        returns (uint256)
    {
        return token == ETH_ADDRESS ? who.balance : IERC20(token).balanceOf(who);
    }

    function _delegate(address executor, bytes memory call)
        private
        returns (bytes memory res)
    {
        bool ok;
        (ok, res) = executor.delegatecall(call);
        if (!ok) {
            if (res.length == 0) revert SwapReverted(executor);
            assembly ("memory-safe") {
                revert(add(res, 0x20), mload(res))
            }
        }
    }

    function _check(address e) private view {
        if (
            e == address(0) || e != E0 && e != E1 && e != E2 && e != E3
                && e != E4 && e != E5 && e != E6 && e != E7 && e != E8
                && e != E9 && e != E10 && e != E11
        ) revert BadExecutor(e);
    }
}
