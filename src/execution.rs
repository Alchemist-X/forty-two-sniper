use std::time::Instant;

use alloy::{
    network::TransactionBuilder,
    primitives::{Address, Bytes, U256},
    providers::Provider,
    rpc::types::TransactionRequest,
};
use eyre::Result;
use tracing::{info, warn};

use crate::{
    abi::{FTRouter, IERC20},
    config::Settings,
    rate_limit::RpcRateLimiter,
};

pub async fn buy_market<P>(
    provider: &P,
    settings: &Settings,
    wallet_address: Address,
    market: Address,
    detected_at: Instant,
    rpc_limiter: &RpcRateLimiter,
) -> Result<()>
where
    P: Provider + Clone,
{
    let router_address = settings.router_address()?;
    let amount = settings.buy_amount()?;
    let token_id = settings.outcome_token_id()?;
    let params = FTRouter::SwapParams {
        isMint: true,
        amount,
        isExactIn: true,
        minOutOrMaxIn: settings.min_out_or_max_in()?,
    };

    let router = FTRouter::new(router_address, provider.clone());
    let calldata = router
        .swapSimple(
            market,
            wallet_address,
            token_id,
            params,
            Bytes::new(),
            Bytes::new(),
        )
        .calldata()
        .to_owned();

    let gas_price =
        bumped_gas_price(provider, rpc_limiter, settings.strategy.gas_price_bump_bps).await?;
    let elapsed_ms = detected_at.elapsed().as_millis();

    if settings.strategy.dry_run {
        warn!(
            market = %market,
            token_id = %token_id,
            amount = %amount,
            gas_price,
            elapsed_ms,
            "dry-run enabled; buy transaction not sent"
        );
        return Ok(());
    }

    let tx = TransactionRequest::default()
        .with_to(router_address)
        .with_chain_id(settings.rpc.chain_id)
        .with_input(calldata)
        .with_gas_limit(settings.strategy.gas_limit)
        .with_gas_price(gas_price);

    rpc_limiter.wait().await;
    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    info!(%market, %tx_hash, elapsed_ms, "buy transaction submitted");

    if settings.strategy.wait_for_receipt {
        rpc_limiter.wait().await;
        let receipt = pending.get_receipt().await?;
        info!(
            %market,
            tx_hash = %receipt.transaction_hash,
            block_number = ?receipt.block_number,
            status = receipt.status(),
            "buy transaction included"
        );
    }

    Ok(())
}

pub async fn approve_router<P>(
    provider: &P,
    settings: &Settings,
    wallet_address: Address,
    infinite: bool,
    rpc_limiter: &RpcRateLimiter,
) -> Result<()>
where
    P: Provider + Clone,
{
    let token = settings.collateral_address()?;
    let router = settings.router_address()?;
    let erc20 = IERC20::new(token, provider.clone());
    rpc_limiter.wait().await;
    let allowance = erc20.allowance(wallet_address, router).call().await?;

    let amount = if infinite {
        U256::MAX
    } else {
        settings.buy_amount()?
    };

    if allowance >= amount {
        info!(%token, %router, allowance = %allowance, "router allowance already sufficient");
        return Ok(());
    }

    let calldata = erc20.approve(router, amount).calldata().to_owned();
    let gas_price =
        bumped_gas_price(provider, rpc_limiter, settings.strategy.gas_price_bump_bps).await?;

    if settings.strategy.dry_run {
        warn!(
            %token,
            %router,
            amount = %amount,
            gas_price,
            "dry-run enabled; approve transaction not sent"
        );
        return Ok(());
    }

    let tx = TransactionRequest::default()
        .with_to(token)
        .with_chain_id(settings.rpc.chain_id)
        .with_input(calldata)
        .with_gas_limit(80_000)
        .with_gas_price(gas_price);

    rpc_limiter.wait().await;
    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    info!(%tx_hash, "approve transaction submitted");

    rpc_limiter.wait().await;
    let receipt = pending.get_receipt().await?;
    info!(
        tx_hash = %receipt.transaction_hash,
        block_number = ?receipt.block_number,
        status = receipt.status(),
        "approve transaction included"
    );

    Ok(())
}

async fn bumped_gas_price<P>(
    provider: &P,
    rpc_limiter: &RpcRateLimiter,
    bump_bps: u64,
) -> Result<u128>
where
    P: Provider,
{
    rpc_limiter.wait().await;
    let gas_price = provider.get_gas_price().await?;
    Ok(gas_price.saturating_mul(10_000 + bump_bps as u128) / 10_000)
}
