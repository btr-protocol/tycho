// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "../TransferManager.sol";

error CapricornExecutor__InvalidDataLength();
error CapricornExecutor__InvalidCallback();

/// Capricorn pAMM pool (Monad). `swap(zeroForOne, amountSpecified > 0 =
/// exact in, amountLimit, recipient, endorsement, data)`: the pool calls its
/// caller back with `(int256 d0, int256 d1, bytes endorsement, bytes data)`
/// (positive = owed to the pool), the caller pays inside the callback, then
/// the pool pays `recipient`. Executed output == `quoteExactIn(tokenIn,
/// amountIn)` at the same state. The router pays exactly the hop amount in the
/// callback; a pool that asks for anything else, or a callback from any other
/// address or outside a hop, reverts.
contract CapricornExecutor is IExecutor, ICallback {
    uint256 private constant _DATA_LENGTH = 61;
    // swap(bool,int256,uint256,address,bytes,bytes)
    bytes4 private constant _SWAP = 0x609c4586;
    // the pool's callback (int256,int256,bytes,bytes)
    bytes4 private constant _CALLBACK = 0xf29bbfbe;

    // keccak256("CapricornExecutor#POOL"), keccak256("CapricornExecutor#AMOUNT")
    bytes32 private constant _POOL_SLOT =
        0x97b4d769864820c2f1d1efaaebeaccc8219d904334a179ff3795a3e3a9cea892;
    bytes32 private constant _AMOUNT_SLOT =
        0x6fb8960f94abc24141136600df3b2885324e6f6e9a697c5f484e28241a392a91;

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
        (address pool,,, bool zeroForOne) = _decodeData(data);
        bytes32 ps = _POOL_SLOT;
        bytes32 as_ = _AMOUNT_SLOT;
        // slither-disable-next-line assembly
        assembly ("memory-safe") {
            if shr(255, amountIn) { revert(0, 0) } // int256 overflow
            tstore(ps, pool)
            tstore(as_, amountIn)
            let p := mload(0x40)
            mstore(p, _SWAP)
            mstore(add(p, 0x04), zeroForOne)
            mstore(add(p, 0x24), amountIn) // > 0: exact in
            mstore(add(p, 0x44), 1) // amountLimit: the router checks min out
            mstore(add(p, 0x64), receiver)
            mstore(add(p, 0x84), 0xc0) // endorsement offset
            mstore(add(p, 0xa4), 0xe0) // data offset
            mstore(add(p, 0xc4), 0) // endorsement = ""
            mstore(add(p, 0xe4), 0) // data = ""
            if iszero(call(gas(), pool, 0, p, 0x104, 0, 0)) {
                returndatacopy(p, 0, returndatasize())
                revert(p, returndatasize())
            }
            tstore(ps, 0)
            tstore(as_, 0)
        }
    }

    /// Delegatecalled by the router after it paid `caller`: only the pool
    /// this hop called, once, owed exactly the hop amount on one side.
    function handleCallback(bytes calldata data)
        public
        returns (bytes memory)
    {
        (int256 d0, int256 d1) = _decodeCallback(data);
        bytes32 ps = _POOL_SLOT;
        bytes32 as_ = _AMOUNT_SLOT;
        address pool;
        uint256 amountIn;
        // slither-disable-next-line assembly
        assembly ("memory-safe") {
            pool := tload(ps)
            amountIn := tload(as_)
            tstore(ps, 0) // one-shot
        }
        bool ok = d0 > 0 && uint256(d0) == amountIn && d1 <= 0
            || d1 > 0 && uint256(d1) == amountIn && d0 <= 0;
        if (pool == address(0) || msg.sender != pool || !ok) {
            revert CapricornExecutor__InvalidCallback();
        }
        return "";
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
            bool zeroForOne
        )
    {
        if (data.length != _DATA_LENGTH) {
            revert CapricornExecutor__InvalidDataLength();
        }
        pool = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
        zeroForOne = uint8(data[60]) > 0;
    }

    function _decodeCallback(bytes calldata data)
        internal
        pure
        returns (int256 d0, int256 d1)
    {
        if (data.length < 68 || bytes4(data[:4]) != _CALLBACK) {
            revert CapricornExecutor__InvalidCallback();
        }
        d0 = int256(uint256(bytes32(data[4:36])));
        d1 = int256(uint256(bytes32(data[36:68])));
    }
}
