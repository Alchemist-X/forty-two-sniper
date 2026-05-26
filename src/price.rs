use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy::{
    primitives::{Address, Bytes, U256},
    providers::Provider,
};
use eyre::{Result, eyre};
use serde::Serialize;
use serde_json::json;
use tokio::{fs::OpenOptions, io::AsyncWriteExt};
use tracing::{debug, info, warn};

use crate::{
    abi::{FTCurve, FTMarket},
    config::Settings,
    rate_limit::RpcRateLimiter,
};

#[derive(Debug, Clone)]
pub enum SellAmount {
    Exact(U256),
    WalletBalance(Address),
}

#[derive(Debug, Clone, Serialize)]
pub struct SellQuote {
    pub market: Address,
    pub token_id: U256,
    pub ot_decimals: u8,
    pub ot_amount: U256,
    pub pre_supply: U256,
    pub post_supply: U256,
    pub pre_marginal_price: U256,
    pub post_marginal_price: U256,
    pub spot_collateral_value: U256,
    pub collateral_to_user: U256,
    pub collateral_to_treasury: U256,
    pub extra_sell_tax: U256,
    pub estimated_net_to_user: U256,
    pub slippage_bps: u64,
    pub protocol_tax_bps: u64,
    pub extra_tax_bps: u64,
    pub effective_loss_bps: u64,
    pub executable: bool,
    pub reason: String,
}

pub struct PriceEngine<P> {
    provider: P,
    settings: Settings,
    rpc_limiter: RpcRateLimiter,
}

impl<P> PriceEngine<P>
where
    P: Provider + Clone,
{
    pub fn new(provider: P, settings: Settings, rpc_limiter: RpcRateLimiter) -> Self {
        Self {
            provider,
            settings,
            rpc_limiter,
        }
    }

    pub async fn quote_sell_exact_ot(
        &self,
        market: Address,
        token_id: U256,
        amount: SellAmount,
    ) -> Result<SellQuote> {
        let market_contract = FTMarket::new(market, self.provider.clone());

        self.rpc_limiter.wait().await;
        let decimals = market_contract.decimals(token_id).call().await?;

        let ot_amount = match amount {
            SellAmount::Exact(amount) => amount,
            SellAmount::WalletBalance(wallet) => {
                self.rpc_limiter.wait().await;
                market_contract.balanceOf(wallet, token_id).call().await?
            }
        };

        if ot_amount.is_zero() {
            return Err(eyre!("OT amount is zero; no sell quote can be produced"));
        }

        self.rpc_limiter.wait().await;
        let deploy = market_contract.readMarketDeployParams().call().await?;
        let curve_address = deploy.curve;
        let curve = FTCurve::new(curve_address, self.provider.clone());

        self.rpc_limiter.wait().await;
        let pre_supply = market_contract.totalSupply(token_id).call().await?;

        if ot_amount > pre_supply {
            return Err(eyre!("OT amount exceeds current token supply"));
        }

        self.rpc_limiter.wait().await;
        let pre_marginal_price = curve.calMarginalPrice(market, token_id).call().await?;

        self.rpc_limiter.wait().await;
        let redeem_quote = curve
            .calRedeemValueByOtDelta(market, token_id, ot_amount, Bytes::new())
            .call()
            .await?;

        let post_supply = pre_supply - ot_amount;
        self.rpc_limiter.wait().await;
        let post_marginal_price = curve.simMarginalPrice(post_supply).call().await?;

        let scale = pow10(decimals);
        let spot_collateral_value = checked_mul_div(ot_amount, pre_marginal_price, scale);
        let gross_redeem = redeem_quote.collateralToUser + redeem_quote.collateralToTreasury;
        let slippage_bps = loss_bps(spot_collateral_value, gross_redeem);
        let protocol_tax_bps = ratio_bps(redeem_quote.collateralToTreasury, gross_redeem);
        let extra_tax = checked_mul_div(
            redeem_quote.collateralToUser,
            U256::from(self.settings.pricing.extra_sell_tax_bps),
            U256::from(10_000),
        );
        let estimated_net_to_user = redeem_quote.collateralToUser.saturating_sub(extra_tax);
        let effective_loss_bps = loss_bps(spot_collateral_value, estimated_net_to_user);
        let executable = slippage_bps <= self.settings.pricing.max_sell_slippage_bps;
        let reason = if executable {
            "ok".to_owned()
        } else {
            format!(
                "slippage {} bps exceeds max {} bps",
                slippage_bps, self.settings.pricing.max_sell_slippage_bps
            )
        };

        Ok(SellQuote {
            market,
            token_id,
            ot_decimals: decimals,
            ot_amount,
            pre_supply,
            post_supply,
            pre_marginal_price,
            post_marginal_price,
            spot_collateral_value,
            collateral_to_user: redeem_quote.collateralToUser,
            collateral_to_treasury: redeem_quote.collateralToTreasury,
            extra_sell_tax: extra_tax,
            estimated_net_to_user,
            slippage_bps,
            protocol_tax_bps,
            extra_tax_bps: self.settings.pricing.extra_sell_tax_bps,
            effective_loss_bps,
            executable,
            reason,
        })
    }

    pub async fn log_sell_quote(
        &self,
        context: &str,
        sample_offset_ms: Option<u64>,
        quote: &SellQuote,
    ) -> Result<()> {
        let record = json!({
            "ts_ms": now_ms(),
            "context": context,
            "sample_offset_ms": sample_offset_ms,
            "market": quote.market.to_string(),
            "token_id": quote.token_id.to_string(),
            "ot_decimals": quote.ot_decimals,
            "ot_amount": quote.ot_amount.to_string(),
            "pre_supply": quote.pre_supply.to_string(),
            "post_supply": quote.post_supply.to_string(),
            "pre_marginal_price": quote.pre_marginal_price.to_string(),
            "post_marginal_price": quote.post_marginal_price.to_string(),
            "spot_collateral_value": quote.spot_collateral_value.to_string(),
            "collateral_to_user": quote.collateral_to_user.to_string(),
            "collateral_to_treasury": quote.collateral_to_treasury.to_string(),
            "extra_sell_tax": quote.extra_sell_tax.to_string(),
            "estimated_net_to_user": quote.estimated_net_to_user.to_string(),
            "slippage_bps": quote.slippage_bps,
            "protocol_tax_bps": quote.protocol_tax_bps,
            "extra_tax_bps": quote.extra_tax_bps,
            "effective_loss_bps": quote.effective_loss_bps,
            "executable": quote.executable,
            "reason": quote.reason,
        });
        append_jsonl(&self.settings.pricing.log_path, &record).await
    }

    pub async fn log_skip(
        &self,
        context: &str,
        market: Address,
        token_id: U256,
        sample_offset_ms: Option<u64>,
        reason: &str,
    ) -> Result<()> {
        let record = json!({
            "ts_ms": now_ms(),
            "context": context,
            "sample_offset_ms": sample_offset_ms,
            "market": market.to_string(),
            "token_id": token_id.to_string(),
            "skipped": true,
            "reason": reason,
        });
        append_jsonl(&self.settings.pricing.log_path, &record).await
    }
}

pub fn spawn_post_buy_sampler<P>(
    provider: P,
    settings: Settings,
    wallet_address: Address,
    market: Address,
    rpc_limiter: RpcRateLimiter,
) where
    P: Provider + Clone + Send + Sync + 'static,
{
    if !settings.pricing.enabled || settings.strategy.dry_run {
        return;
    }

    let offsets = settings.pricing.sample_offsets_ms.clone();
    tokio::spawn(async move {
        let token_id = match settings.outcome_token_id() {
            Ok(token_id) => token_id,
            Err(err) => {
                warn!(?err, "post-buy price sampler skipped");
                return;
            }
        };
        let engine = PriceEngine::new(provider, settings, rpc_limiter);

        for offset_ms in offsets {
            if offset_ms > 0 {
                tokio::time::sleep(Duration::from_millis(offset_ms)).await;
            }

            match engine
                .quote_sell_exact_ot(market, token_id, SellAmount::WalletBalance(wallet_address))
                .await
            {
                Ok(quote) => {
                    if !quote.executable {
                        warn!(
                            market = %market,
                            token_id = %token_id,
                            slippage_bps = quote.slippage_bps,
                            reason = %quote.reason,
                            "post-buy sell quote blocked by slippage guard"
                        );
                    }
                    if let Err(err) = engine
                        .log_sell_quote("post_buy_price_sample", Some(offset_ms), &quote)
                        .await
                    {
                        warn!(?err, "failed to write post-buy price sample");
                    }
                    debug!(
                        market = %market,
                        token_id = %token_id,
                        offset_ms,
                        slippage_bps = quote.slippage_bps,
                        "post-buy price sample recorded"
                    );
                }
                Err(err) => {
                    warn!(market = %market, token_id = %token_id, ?err, "post-buy price sample failed");
                    let _ = engine
                        .log_skip(
                            "post_buy_price_sample",
                            market,
                            token_id,
                            Some(offset_ms),
                            &err.to_string(),
                        )
                        .await;
                }
            }
        }

        info!(market = %market, token_id = %token_id, "post-buy price sampler finished");
    });
}

async fn append_jsonl(path: &str, record: &serde_json::Value) -> Result<()> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(serde_json::to_string(record)?.as_bytes())
        .await?;
    file.write_all(b"\n").await?;
    Ok(())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn pow10(decimals: u8) -> U256 {
    let mut value = U256::from(1);
    for _ in 0..decimals {
        value *= U256::from(10);
    }
    value
}

fn checked_mul_div(value: U256, multiplier: U256, divisor: U256) -> U256 {
    if divisor.is_zero() {
        return U256::ZERO;
    }

    value.saturating_mul(multiplier) / divisor
}

fn loss_bps(reference: U256, actual: U256) -> u64 {
    if reference.is_zero() || actual >= reference {
        return 0;
    }

    ratio_bps(reference - actual, reference)
}

fn ratio_bps(numerator: U256, denominator: U256) -> u64 {
    if denominator.is_zero() {
        return 0;
    }

    let value = numerator.saturating_mul(U256::from(10_000)) / denominator;
    let capped = value.min(U256::from(u64::MAX));
    capped.to::<u64>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_loss_bps() {
        assert_eq!(loss_bps(U256::from(100), U256::from(75)), 2_500);
        assert_eq!(loss_bps(U256::from(100), U256::from(100)), 0);
        assert_eq!(loss_bps(U256::from(100), U256::from(110)), 0);
    }

    #[test]
    fn computes_units_scale() {
        assert_eq!(pow10(6), U256::from(1_000_000));
    }
}
