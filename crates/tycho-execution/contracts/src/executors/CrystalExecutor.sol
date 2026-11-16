// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

error CrystalExecutor__InvalidDataLength();

/// Crystal (Monad CLOB; the WMON/USDC book is quoted by the Crystal Vault).
/// Taker entry `swapExactTokensForTokens(amountIn, amountOutMin, path, to,
/// deadline, referrer)` on the Crystal core, which resolves the market from
/// the path. Pull model: the router approves the core for `amountIn` and
/// revokes after the hop. Output is paid straight to `receiver`; the router
/// enforces the caller's minimum on its balance diff, so the venue minimum is
/// 0. Executed output == `Crystal.getQuote(market, isBuy, true, false, size,
/// worstPrice)` at the same state.
contract CrystalExecutor is IExecutor {
    address internal constant CRYSTAL =
        0x508254c838B2e936B0631440c5C6E3AB3a4a98BD;
    // swapExactTokensForTokens(uint256,uint256,address[],address,uint256,address)
    bytes4 internal constant _SWAP = 0x05a1450d;
    uint256 internal constant _DATA_LENGTH = 40;

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
        (address tokenIn, address tokenOut) = _decodeData(data);
        // slither-disable-next-line assembly
        assembly ("memory-safe") {
            let p := mload(0x40)
            mstore(p, _SWAP)
            mstore(add(p, 0x04), amountIn)
            mstore(add(p, 0x24), 0) // amountOutMin: the router checks
            mstore(add(p, 0x44), 0xc0) // path offset
            mstore(add(p, 0x64), receiver)
            mstore(add(p, 0x84), timestamp()) // deadline
            mstore(add(p, 0xa4), 0) // referrer
            mstore(add(p, 0xc4), 2) // path length
            mstore(add(p, 0xe4), tokenIn)
            mstore(add(p, 0x104), tokenOut)
            if iszero(call(gas(), CRYSTAL, 0, p, 0x124, 0, 0)) {
                returndatacopy(p, 0, returndatasize())
                revert(p, returndatasize())
            }
        }
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
        (tokenIn, tokenOut) = _decodeData(data);
        return (
            TransferManager.TransferType.ProtocolWillDebit,
            CRYSTAL,
            tokenIn,
            tokenOut,
            false
        );
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address tokenIn, address tokenOut)
    {
        if (data.length != _DATA_LENGTH) {
            revert CrystalExecutor__InvalidDataLength();
        }
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
    }
}
