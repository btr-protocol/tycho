// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

interface IHanjiProxy {
    function getConfig()
        external
        view
        returns (
            uint256 scalingFactorTokenX,
            uint256 scalingFactorTokenY,
            address tokenX,
            address tokenY,
            bool supportsNativeEth,
            bool isTokenXWeth,
            address askTrie,
            address bidTrie,
            uint64 adminCommissionRate,
            uint64 totalAggressiveCommissionRate,
            uint64 totalPassiveCommissionRate,
            uint64 passiveOrderPayoutRate,
            bool shouldInvokeOnTrade
        );

    function placeOrder(
        bool isAsk,
        uint128 quantity,
        uint72 price,
        uint128 maxCommission,
        bool marketOnly,
        bool postOnly,
        bool transferExecutedTokens,
        uint256 expires
    ) external payable returns (uint64, uint128, uint128, uint128);

    function placeMarketOrderWithTargetValue(
        bool isAsk,
        uint128 targetTokenYValue,
        uint72 price,
        uint128 maxCommission,
        bool transferExecutedTokens,
        uint256 expires
    ) external payable returns (uint128, uint128, uint128);
}

interface IWrappedNative {
    function deposit() external payable;
}

error HanjiExecutor__InvalidDataLength();
error HanjiExecutor__TokensNotInMarket();
error HanjiExecutor__PartialFill();

/// @title HanjiExecutor
/// @notice Swaps on Hanji through a market's fast-quoter proxy, which has the
/// LP manager quote just in time and fills the order against the book.
/// @dev Protocol data: `proxy (20) | tokenIn (20) | tokenOut (20)`. Selling
/// token X is a market ask of `amountIn / scalingX` shares; selling token Y is
/// a market bid for a target Y value. Input below one share (sell X) or left
/// over from the last share (sell Y) stays with the router. Markets on the
/// native coin pay it out unwrapped; it is wrapped so the router receives
/// tokenOut.
contract HanjiExecutor is IExecutor {
    /// @dev Widest price the market accepts (FP24: 999999 * 10^15).
    uint72 internal constant _MAX_PRICE = 999_999_000_000_000_000_000;

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    // The router enforces the user's minAmountOut and measures the output via
    // balance diff, so the orders carry no price limit and no fee cap.
    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address)
        external
        payable
    {
        (address proxy, address tokenIn, address tokenOut) = _decodeData(data);
        (
            uint256 scalingX,
            uint256 scalingY,
            address tokenX,
            address tokenY,,,,,,,,,
        ) = IHanjiProxy(proxy).getConfig();

        uint256 nativeBefore = address(this).balance;
        if (tokenIn == tokenX && tokenOut == tokenY) {
            uint256 shares = amountIn / scalingX;
            (, uint128 executed,,) = IHanjiProxy(proxy)
                .placeOrder(
                    true,
                    uint128(shares),
                    1,
                    type(uint128).max,
                    true,
                    false,
                    true,
                    block.timestamp
                );
            if (executed != shares) revert HanjiExecutor__PartialFill();
        } else if (tokenIn == tokenY && tokenOut == tokenX) {
            uint256 target = amountIn / scalingY;
            (uint128 executed, uint128 value, uint128 fee) = IHanjiProxy(proxy)
                .placeMarketOrderWithTargetValue(
                    false,
                    uint128(target),
                    _MAX_PRICE,
                    type(uint128).max,
                    true,
                    block.timestamp
                );
            // Up to one share's cost left unspent is share rounding, as in the
            // simulation; more means the book ran out.
            uint256 cost = uint256(value) + fee;
            if (
                executed == 0 || cost > target
                    || (target - cost) * executed >= cost
            ) {
                revert HanjiExecutor__PartialFill();
            }
        } else {
            revert HanjiExecutor__TokensNotInMarket();
        }
        uint256 native = address(this).balance - nativeBefore;
        if (native != 0) {
            IWrappedNative(tokenOut).deposit{value: native}();
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
        (receiver, tokenIn, tokenOut) = _decodeData(data);
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        outputToRouter = true;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address proxy, address tokenIn, address tokenOut)
    {
        if (data.length != 60) {
            revert HanjiExecutor__InvalidDataLength();
        }
        proxy = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
    }
}
