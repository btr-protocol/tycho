// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";
import {
    IERC20,
    SafeERC20
} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";

/// @title HanjiSwapAdapter
/// @notice Adapter for Hanji, an on-chain CLOB whose taker flow goes through
/// a per-market fast-quoter proxy: the proxy has the LP manager post
/// oracle-bounded quotes into the market just in time, then fills the taker
/// against them.
/// @dev The pool id is the proxy address (left-aligned). Exact-input only.
/// Selling token X is a market ask of `amount / scalingX` shares; selling
/// token Y is a market bid for a target Y value. Amounts below one share are
/// left with the caller, as on-chain.
contract HanjiSwapAdapter is ISwapAdapter {
    using SafeERC20 for IERC20;

    /// @dev Widest price the market accepts (FP24: 999999 * 10^15).
    uint72 constant MAX_PRICE = 999_999_000_000_000_000_000;

    /// @dev Carries a probe order's spent input out of the reverted call.
    error HanjiSwapAdapter__Depth(uint256 spent);

    /// @dev Receives the native coin a market on it pays out.
    receive() external payable {}

    /// @inheritdoc ISwapAdapter
    function price(bytes32, address, address, uint256[] memory)
        external
        pure
        override
        returns (Fraction[] memory)
    {
        revert NotImplemented("HanjiSwapAdapter.price");
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Reverts `LimitExceeded` when the book cannot fill the whole amount.
    function swap(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        OrderSide side,
        uint256 specifiedAmount
    ) external override returns (Trade memory trade) {
        if (side == OrderSide.Buy) {
            revert NotImplemented("HanjiSwapAdapter: exact-input only");
        }
        uint256 gasBefore = gasleft();
        (uint256 spent, uint256 unfilled, uint256 amountOut) =
            _order(msg.sender, poolId, sellToken, buyToken, specifiedAmount);
        if (unfilled != 0) revert LimitExceeded(spent);
        trade.calculatedAmount = amountOut;
        trade.gasUsed = gasBefore - gasleft();
        trade.price = Fraction(amountOut, specifiedAmount);
    }

    /// @inheritdoc ISwapAdapter
    /// @dev The sell limit is the book's depth: what one order for far more
    /// than the book holds actually spends, funded by the caller like a swap
    /// and reverted. The buy limit is what the LP manager and the resting book
    /// hold of the buy token.
    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        override
        returns (uint256[] memory limits)
    {
        IHanjiProxy proxy = _proxy(poolId);
        limits = new uint256[](2);
        limits[1] = IERC20(buyToken).balanceOf(proxy.lpManagerAddress())
            + IERC20(buyToken).balanceOf(proxy.lobAddress());
        (uint256 scalingX, uint256 scalingY, address tokenX,) = _config(proxy);
        uint256 amount = (sellToken == tokenX ? scalingX : scalingY) << 96;
        uint256 funds = IERC20(sellToken).balanceOf(msg.sender);
        uint256 allowed = IERC20(sellToken).allowance(msg.sender, address(this));
        if (funds < amount) amount = funds;
        if (allowed < amount) amount = allowed;
        try this.depthAndRevert(
            msg.sender, poolId, sellToken, buyToken, amount
        ) {}
        catch (bytes memory reason) {
            if (
                reason.length == 36
                    && bytes4(reason) == HanjiSwapAdapter__Depth.selector
            ) {
                limits[0] = uint256(bytes32(_tail(reason)));
            }
        }
    }

    /// @notice Places an order funded by `payer` and reverts with the amount
    /// of the sell token it spent.
    /// @dev Only callable by this adapter; used by {getLimits}.
    function depthAndRevert(
        address payer,
        bytes32 poolId,
        address sellToken,
        address buyToken,
        uint256 amount
    ) external {
        require(msg.sender == address(this));
        (uint256 spent,,) = _order(payer, poolId, sellToken, buyToken, amount);
        revert HanjiSwapAdapter__Depth(spent);
    }

    /// @dev Sells up to `amount` pulled from `payer` as one market order and
    /// pays the output and any unspent input back to `payer`. `unfilled` is
    /// non-zero when the book ran out: shares left unsold on an ask, or more
    /// than one share's cost left unspent on a bid (less is share rounding).
    function _order(
        address payer,
        bytes32 poolId,
        address sellToken,
        address buyToken,
        uint256 amount
    ) internal returns (uint256 spent, uint256 unfilled, uint256 amountOut) {
        IHanjiProxy proxy = _proxy(poolId);
        (uint256 scalingX, uint256 scalingY, address tokenX, address tokenY) =
            _config(proxy);
        uint256 nativeBefore = address(this).balance;

        if (sellToken == tokenX && buyToken == tokenY) {
            uint256 shares = amount / scalingX;
            if (shares == 0) revert TooSmall(scalingX);
            IERC20(tokenX)
                .safeTransferFrom(payer, address(this), shares * scalingX);
            IERC20(tokenX).forceApprove(address(proxy), shares * scalingX);
            (, uint128 executed, uint128 value, uint128 fee) = proxy.placeOrder(
                true,
                uint128(shares),
                1,
                type(uint128).max,
                true,
                false,
                true,
                block.timestamp
            );
            spent = uint256(executed) * scalingX;
            unfilled = shares - executed;
            amountOut = (uint256(value) - fee) * scalingY;
        } else if (sellToken == tokenY && buyToken == tokenX) {
            uint256 target = amount / scalingY;
            if (target == 0) revert TooSmall(scalingY);
            IERC20(tokenY).safeTransferFrom(payer, address(this), amount);
            IERC20(tokenY).forceApprove(address(proxy), amount);
            (uint128 executed, uint128 value, uint128 fee) = proxy.placeMarketOrderWithTargetValue(
                false,
                uint128(target),
                MAX_PRICE,
                type(uint128).max,
                true,
                block.timestamp
            );
            if (executed == 0) revert TooSmall(amount);
            uint256 cost = uint256(value) + fee;
            spent = cost * scalingY;
            if (cost > target) {
                unfilled = cost - target;
            } else if ((target - cost) * executed >= cost) {
                unfilled = target - cost;
            }
            amountOut = uint256(executed) * scalingX;
        } else {
            revert Unavailable("HanjiSwapAdapter: tokens not in market");
        }

        IERC20(sellToken).forceApprove(address(proxy), 0);
        // Markets on the native coin pay it out unwrapped.
        uint256 native = address(this).balance - nativeBefore;
        if (native != 0) IWrappedNative(buyToken).deposit{value: native}();
        uint256 left = IERC20(sellToken).balanceOf(address(this));
        if (left != 0) IERC20(sellToken).safeTransfer(payer, left);
        IERC20(buyToken).safeTransfer(payer, amountOut);
    }

    /// @inheritdoc ISwapAdapter
    function getCapabilities(bytes32, address, address)
        external
        pure
        override
        returns (Capability[] memory capabilities)
    {
        capabilities = new Capability[](1);
        capabilities[0] = Capability.SellOrder;
    }

    /// @inheritdoc ISwapAdapter
    function getTokens(bytes32 poolId)
        external
        view
        override
        returns (address[] memory tokens)
    {
        tokens = new address[](2);
        (,, tokens[0], tokens[1]) = _config(_proxy(poolId));
    }

    /// @inheritdoc ISwapAdapter
    function getPoolIds(uint256, uint256)
        external
        pure
        override
        returns (bytes32[] memory)
    {
        revert NotImplemented("HanjiSwapAdapter.getPoolIds");
    }

    /// @dev `reason` without its 4-byte selector.
    function _tail(bytes memory reason) internal pure returns (bytes memory) {
        bytes memory tail = new bytes(reason.length - 4);
        for (uint256 i = 0; i < tail.length; i++) {
            tail[i] = reason[i + 4];
        }
        return tail;
    }

    function _proxy(bytes32 poolId) internal pure returns (IHanjiProxy) {
        return IHanjiProxy(address(bytes20(poolId)));
    }

    function _config(IHanjiProxy proxy)
        internal
        view
        returns (
            uint256 scalingX,
            uint256 scalingY,
            address tokenX,
            address tokenY
        )
    {
        (scalingX, scalingY, tokenX, tokenY,,,,,,,,,) = proxy.getConfig();
    }
}

interface IWrappedNative {
    function deposit() external payable;
}

/// @dev The fast-quoter proxy: the market's taker interface plus its wiring.
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

    function lobAddress() external view returns (address);

    function lpManagerAddress() external view returns (address);

    function placeOrder(
        bool isAsk,
        uint128 quantity,
        uint72 price,
        uint128 maxCommission,
        bool marketOnly,
        bool postOnly,
        bool transferExecutedTokens,
        uint256 expires
    )
        external
        payable
        returns (
            uint64 orderId,
            uint128 executedShares,
            uint128 executedValue,
            uint128 aggressiveFee
        );

    function placeMarketOrderWithTargetValue(
        bool isAsk,
        uint128 targetTokenYValue,
        uint72 price,
        uint128 maxCommission,
        bool transferExecutedTokens,
        uint256 expires
    )
        external
        payable
        returns (
            uint128 executedShares,
            uint128 executedValue,
            uint128 aggressiveFee
        );
}
