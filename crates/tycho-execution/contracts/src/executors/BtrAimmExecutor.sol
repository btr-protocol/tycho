// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

interface IBtrAimmPool {
    function swap_qe(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address recipient,
        uint256 deadline
    ) external payable returns (uint256 out);
}

error BtrAimmExecutor__InvalidDataLength();
error BtrAimmExecutor__NativeUnsupported();

/// Swaps through a BTR AIMM pool's public taker entry `swap_qe` (ERC-20 legs;
/// native travels as WMON). The pool pulls `amountIn` from its caller (the
/// router, via approval) and pays `receiver` directly. The pool is
/// permissioned while a leg is `SWAP_GATED`: the router needs AC lane 0x400
/// or the call reverts `NotAuthorized`.
/// data (60 bytes, packed): tokenIn | tokenOut | pool.
// forge-lint: disable-next-line(locked-ether)
contract BtrAimmExecutor is IExecutor {
    uint256 internal constant _DATA_LENGTH = 60;

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address tokenIn, address tokenOut, address pool) = _decodeData(data);
        // minAmountOut is the router's; the deadline is the tx's.
        // slither-disable-next-line unused-return
        // forge-lint: disable-next-line(unused-return)
        IBtrAimmPool(pool)
            .swap_qe(
                tokenIn, tokenOut, amountIn, 0, receiver, type(uint256).max
            );
    }

    function getTransferData(bytes calldata data)
        external
        pure
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        address pool;
        (tokenIn, tokenOut, pool) = _decodeData(data);
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        receiver = pool;
        outputToRouter = false;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address tokenIn, address tokenOut, address pool)
    {
        if (data.length != _DATA_LENGTH) {
            revert BtrAimmExecutor__InvalidDataLength();
        }
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        pool = address(bytes20(data[40:60]));
        if (tokenIn == ETH_ADDRESS || tokenOut == ETH_ADDRESS) {
            revert BtrAimmExecutor__NativeUnsupported();
        }
    }
}
