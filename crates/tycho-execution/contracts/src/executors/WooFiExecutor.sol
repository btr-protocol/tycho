// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

// WOOFi sPMM (WooPPV2) swap. Push model: the input is transferred to the pool
// before the call; no approval, no callback.
interface IWooPPV2 {
    function swap(
        address fromToken,
        address toToken,
        uint256 fromAmount,
        uint256 minToAmount,
        address to,
        address rebateTo
    ) external returns (uint256 realToAmount);
}

error WooFiExecutor__InvalidDataLength();

/// The router transfers `amountIn` to the pool (`TransferType.Transfer`); the
/// pool pays the receiver. The router enforces the caller's minimum on the
/// receiver's balance diff, so the venue minimum is 0.
contract WooFiExecutor is IExecutor {
    uint256 internal constant _DATA_LENGTH = 60;

    function fundsExpectedAddress(bytes calldata data)
        external
        pure
        returns (address receiver)
    {
        (receiver,,) = _decodeData(data);
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address pool, address tokenIn, address tokenOut) = _decodeData(data);
        // slither-disable-next-line unused-return
        IWooPPV2(pool).swap(tokenIn, tokenOut, amountIn, 0, receiver, address(0));
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
        (receiver, tokenIn, tokenOut) = _decodeData(data);
        transferType = TransferManager.TransferType.Transfer;
        outputToRouter = false;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address pool, address tokenIn, address tokenOut)
    {
        if (data.length != _DATA_LENGTH) {
            revert WooFiExecutor__InvalidDataLength();
        }
        pool = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
    }
}
