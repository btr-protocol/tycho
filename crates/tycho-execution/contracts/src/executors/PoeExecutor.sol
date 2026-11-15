// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "../TransferManager.sol";

// LFJ POE pool (Monad). The pool pays `recipient` first, then calls
// `swapCallback(int256,int256,bytes)` on the caller with the pool's token X and
// Y deltas (positive = owed to the pool); the caller pays the input to the pool
// inside the callback.
interface IPoePool {
    function swap(
        address recipient,
        bool swapXtoY,
        uint256 amountIn,
        bytes calldata data
    ) external returns (int256 amountXDelta, int256 amountYDelta);
}

error PoeExecutor__InvalidDataLength();
error PoeExecutor__InvalidCallback();

/// The router pays the hop's full `amountIn` in the callback; a pool that owes
/// less (partial fill) or more reverts instead of keeping the difference.
contract PoeExecutor is IExecutor, ICallback {
    uint256 private constant _DATA_LENGTH = 61;

    // swapCallback(int256,int256,bytes)
    bytes4 private constant _CALLBACK_SELECTOR = 0xfa483e72;

    // keccak256("PoeExecutor#CURRENT_POOL"), keccak256("PoeExecutor#AMOUNT_IN")
    bytes32 private constant _POOL_SLOT =
        0x48766b45cb26af0f27ceed619450d89815cc8e8a5a47c1e980f62aea1880636c;
    bytes32 private constant _AMOUNT_SLOT =
        0x7597b4523d257a3a85c083d1a3fab4363787bf19ea464162752f014dd17da143;

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
        (address pool,,, bool swapXtoY) = _decodeData(data);
        _store(pool, amountIn);
        // The returned deltas are unused: the router measures the receiver's
        // balance diff.
        // slither-disable-next-line unused-return
        IPoePool(pool).swap(receiver, swapXtoY, amountIn, "");
        _store(address(0), 0);
    }

    function handleCallback(bytes calldata data)
        public
        view
        returns (bytes memory)
    {
        (int256 dX, int256 dY) = _decodeCallback(data);
        (address pool, uint256 amountIn) = _load();
        // Exactly the hop amount is owed on one side, the other is paid out.
        bool ok = dX > 0 && uint256(dX) == amountIn && dY <= 0
            || dY > 0 && uint256(dY) == amountIn && dX <= 0;
        if (msg.sender != pool || !ok) revert PoeExecutor__InvalidCallback();
        // The pool requires its own selector back, left-aligned in one word.
        return abi.encodePacked(_CALLBACK_SELECTOR, bytes28(0));
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
        (, tokenIn, tokenOut,) = _decodeData(data);
        return (
            TransferManager.TransferType.None,
            address(0),
            tokenIn,
            tokenOut,
            false
        );
    }

    function getCallbackTransferData(
        bytes calldata data,
        address, /* tokenIn */
        address caller
    )
        external
        pure
        returns (TransferManager.TransferType transferType, address receiver)
    {
        _decodeCallback(data);
        return (TransferManager.TransferType.Transfer, caller);
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address pool,
            address tokenIn,
            address tokenOut,
            bool swapXtoY
        )
    {
        if (data.length != _DATA_LENGTH) {
            revert PoeExecutor__InvalidDataLength();
        }
        pool = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
        swapXtoY = uint8(data[60]) > 0;
    }

    function _decodeCallback(bytes calldata data)
        internal
        pure
        returns (int256 dX, int256 dY)
    {
        if (data.length < 68 || bytes4(data[:4]) != _CALLBACK_SELECTOR) {
            revert PoeExecutor__InvalidCallback();
        }
        dX = int256(uint256(bytes32(data[4:36])));
        dY = int256(uint256(bytes32(data[36:68])));
    }

    function _store(address pool, uint256 amountIn) internal {
        bytes32 p = _POOL_SLOT;
        bytes32 a = _AMOUNT_SLOT;
        // slither-disable-next-line assembly
        assembly {
            tstore(p, pool)
            tstore(a, amountIn)
        }
    }

    function _load() internal view returns (address pool, uint256 amountIn) {
        bytes32 p = _POOL_SLOT;
        bytes32 a = _AMOUNT_SLOT;
        // slither-disable-next-line assembly
        assembly {
            pool := tload(p)
            amountIn := tload(a)
        }
    }
}
