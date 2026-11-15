// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

// ThogAMM (Monad) maker swap. Pull model: the router approves the AMM for
// `amountIn`, the AMM `transferFrom`s it and pays `recipient`. The deadline is
// a block number.
interface IThogAMM {
    function makerSwapExactInput(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address recipient,
        uint256 deadlineBlock
    ) external returns (uint256 amountOut);
}

error ThogExecutor__InvalidDataLength();

/// The output is paid straight to the receiver; the router enforces the
/// caller's minimum on the receiver's balance diff, so the venue minimum is 0.
contract ThogExecutor is IExecutor {
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
        (address amm, address tokenIn, address tokenOut) = _decodeData(data);
        // slither-disable-next-line unused-return
        IThogAMM(amm)
            .makerSwapExactInput(
                tokenIn, tokenOut, amountIn, 0, receiver, block.number
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
        (receiver, tokenIn, tokenOut) = _decodeData(data);
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        outputToRouter = false;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address amm, address tokenIn, address tokenOut)
    {
        if (data.length != _DATA_LENGTH) {
            revert ThogExecutor__InvalidDataLength();
        }
        amm = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
    }
}
