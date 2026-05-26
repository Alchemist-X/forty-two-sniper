use alloy::sol;

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract FTMarketController {
        event CreateNewMarket(
            address indexed market,
            address collateral,
            uint256 parentTokenId,
            bytes32 questionId,
            address curve,
            uint256 timestampStart
        );

        function getConfig(address market)
            external
            view
            returns (
                address treasury,
                uint80 feeRate,
                uint256 numOutcomes,
                uint128 timestampEnd,
                uint256 answer,
                bool isFinalised
            );
    }

    #[allow(missing_docs)]
    #[sol(rpc)]
    contract FTRouter {
        struct SwapParams {
            bool isMint;
            uint256 amount;
            bool isExactIn;
            uint256 minOutOrMaxIn;
        }

        function swapSimple(
            address market,
            address receiver,
            uint256 tokenId,
            SwapParams params,
            bytes dataSwap,
            bytes dataGuess
        ) external;
    }

    #[allow(missing_docs)]
    #[sol(rpc)]
    contract IERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function balanceOf(address owner) external view returns (uint256);
    }
}
