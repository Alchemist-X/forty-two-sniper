mod abi;
mod config;
mod execution;
mod metadata;
mod price;
mod rate_limit;
mod validation;

use std::{collections::HashSet, path::PathBuf, time::Instant};

use abi::FTMarketController;
use alloy::{
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::{BlockNumberOrTag, Filter},
    signers::local::PrivateKeySigner,
    sol_types::SolEvent,
};
use clap::{Args, Parser, Subcommand};
use config::Settings;
use eyre::{Context, Result, eyre};
use futures_util::StreamExt;
use rate_limit::RpcRateLimiter;
use tracing::{debug, error, info, warn};
use url::Url;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Listen for CreateNewMarket and submit a buy transaction.
    Run,
    /// Approve the router to spend the configured collateral token.
    Approve {
        /// Approve the maximum uint256 amount instead of one configured buy amount.
        #[arg(long)]
        infinite: bool,
    },
    /// Validate and print the resolved configuration without starting the bot.
    CheckConfig,
    /// Compute a sell quote and write the pricing decision log.
    QuoteSell {
        market: String,
        /// Human-readable OT amount. If omitted, quote the configured wallet balance.
        #[arg(long)]
        amount_units: Option<String>,
        /// Wallet to inspect when amount-units is omitted. Defaults to SNIPER_PRIVATE_KEY address.
        #[arg(long)]
        wallet: Option<String>,
    },
    /// Replay historical RedeemSwap logs against block-1 quotes.
    ValidatePricing(ValidatePricingArgs),
    /// Validate existing pricing validation JSONL logs without RPC calls.
    ValidatePricingLog(ValidatePricingLogArgs),
}

#[derive(Debug, Args)]
struct ValidatePricingArgs {
    #[arg(long, default_value_t = 100)]
    samples: usize,
    #[arg(long, default_value_t = 2_000_000)]
    lookback_blocks: u64,
    #[arg(long, default_value_t = 50_000)]
    chunk_size: u64,
    #[arg(long, default_value = "100000000000000")]
    tolerance_wei: String,
    #[arg(long, default_value = "logs/pricing-validation.jsonl")]
    output_path: String,
    /// Optional RPC URL used only for historical eth_getLogs scans.
    #[arg(long)]
    scan_rpc_url: Option<String>,
    /// Number of recent 42Space markets to use as log addresses.
    #[arg(long, default_value_t = 400)]
    market_limit: usize,
    /// Offset into the 42Space REST market list.
    #[arg(long, default_value_t = 0)]
    market_offset: usize,
    /// Number of market addresses per eth_getLogs request.
    #[arg(long, default_value_t = 5)]
    market_batch_size: usize,
    /// Request budget for the offline historical log scan RPC.
    #[arg(long, default_value_t = 5)]
    scan_max_requests_per_second: u32,
    /// REST market ordering used to pick likely active sample sources.
    #[arg(long, default_value = "volume")]
    market_order: String,
    /// Optional REST market status filter, for example live, ended, resolved, finalised.
    #[arg(long)]
    market_status: Option<String>,
}

#[derive(Debug, Args)]
struct ValidatePricingLogArgs {
    /// One or more JSONL files produced by validate-pricing.
    #[arg(required = true)]
    input: Vec<PathBuf>,
    /// Maximum tolerated diff in wei for user and treasury outputs.
    #[arg(long, default_value = "100000000000000")]
    tolerance_wei: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let settings = Settings::load(&cli.config)
        .with_context(|| format!("failed to load {}", cli.config.display()))?;

    match cli.command {
        Command::Run => run(settings).await,
        Command::Approve { infinite } => approve(settings, infinite).await,
        Command::CheckConfig => {
            settings.validate()?;
            println!("{:#?}", settings.redacted_for_display());
            Ok(())
        }
        Command::QuoteSell {
            market,
            amount_units,
            wallet,
        } => {
            quote_sell(
                settings,
                &market,
                amount_units.as_deref(),
                wallet.as_deref(),
            )
            .await
        }
        Command::ValidatePricing(args) => validate_pricing(settings, args).await,
        Command::ValidatePricingLog(args) => validate_pricing_log(args).await,
    }
}

async fn run(settings: Settings) -> Result<()> {
    settings.validate()?;
    let signer = signer_from_env(&settings)?;
    let wallet_address = signer.address();

    let http_provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(settings.rpc.http_url.parse::<Url>()?);
    let rpc_limiter = RpcRateLimiter::new(settings.rpc.max_requests_per_second);

    let ws_provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(settings.rpc.ws_url.clone()))
        .await?;

    let controller = settings.controller_address()?;
    let filter = Filter::new()
        .address(controller)
        .event_signature(FTMarketController::CreateNewMarket::SIGNATURE_HASH)
        .from_block(BlockNumberOrTag::Latest);

    info!(
        controller = %controller,
        router = %settings.router_address()?,
        wallet = %wallet_address,
        dry_run = settings.strategy.dry_run,
        rpc_max_rps = settings.rpc.max_requests_per_second,
        "starting 42Space sniper"
    );

    let sub = ws_provider.subscribe_logs(&filter).await?;
    let mut stream = sub.into_stream();
    let mut seen = HashSet::<Address>::new();
    let metadata_client = metadata::Client::new(settings.metadata.clone());

    loop {
        tokio::select! {
            maybe_log = stream.next() => {
                let Some(log) = maybe_log else {
                    return Err(eyre!("websocket log stream ended"));
                };

                let received_at = Instant::now();
                let decoded = match log.log_decode::<FTMarketController::CreateNewMarket>() {
                    Ok(decoded) => decoded.inner.data,
                    Err(err) => {
                        warn!(?err, "skipping undecodable controller log");
                        continue;
                    }
                };

                if !seen.insert(decoded.market) {
                    debug!(market = %decoded.market, "duplicate market event ignored");
                    continue;
                }

                if !settings.is_allowed_collateral(decoded.collateral) {
                    warn!(
                        market = %decoded.market,
                        collateral = %decoded.collateral,
                        "market ignored because collateral is not allowed"
                    );
                    continue;
                }

                info!(
                    market = %decoded.market,
                    collateral = %decoded.collateral,
                    parent_token_id = %decoded.parentTokenId,
                    question_id = %decoded.questionId,
                    curve = %decoded.curve,
                    timestamp_start = %decoded.timestampStart,
                    "new market detected"
                );

                if settings.metadata.enabled {
                    let client = metadata_client.clone();
                    let market = decoded.market;
                    tokio::spawn(async move {
                        match client.market(market).await {
                            Ok(value) => debug!(%market, metadata = %value, "metadata fetched"),
                            Err(err) => debug!(%market, ?err, "metadata fetch failed"),
                        }
                    });
                }

                if let Err(err) = execution::buy_market(
                    &http_provider,
                    &settings,
                    wallet_address,
                    decoded.market,
                    received_at,
                    &rpc_limiter,
                )
                .await
                {
                    error!(market = %decoded.market, ?err, "buy path failed");
                } else {
                    price::spawn_post_buy_sampler(
                        http_provider.clone(),
                        settings.clone(),
                        wallet_address,
                        decoded.market,
                        rpc_limiter.clone(),
                    );
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                info!("shutdown requested");
                return Ok(());
            }
        }
    }
}

async fn approve(settings: Settings, infinite: bool) -> Result<()> {
    settings.validate()?;
    let signer = signer_from_env(&settings)?;
    let wallet_address = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(settings.rpc.http_url.parse::<Url>()?);
    let rpc_limiter = RpcRateLimiter::new(settings.rpc.max_requests_per_second);

    execution::approve_router(&provider, &settings, wallet_address, infinite, &rpc_limiter).await
}

async fn quote_sell(
    settings: Settings,
    market: &str,
    amount_units: Option<&str>,
    wallet: Option<&str>,
) -> Result<()> {
    settings.validate()?;
    let market = market
        .parse::<Address>()
        .wrap_err("failed to parse market")?;
    let provider = ProviderBuilder::new().connect_http(settings.rpc.http_url.parse::<Url>()?);
    let rpc_limiter = RpcRateLimiter::new(settings.rpc.max_requests_per_second);
    let engine = price::PriceEngine::new(provider, settings.clone(), rpc_limiter);
    let token_id = settings.outcome_token_id()?;

    let amount = if let Some(amount_units) = amount_units {
        let decimals = read_ot_decimals(&settings, market, token_id).await?;
        price::SellAmount::Exact(settings.parse_units("amount_units", amount_units, decimals)?)
    } else {
        let wallet = match wallet {
            Some(wallet) => wallet
                .parse::<Address>()
                .wrap_err("failed to parse wallet")?,
            None => signer_from_env(&settings)?.address(),
        };
        price::SellAmount::WalletBalance(wallet)
    };

    let quote = engine.quote_sell_exact_ot(market, token_id, amount).await?;
    engine.log_sell_quote("quote_sell", None, &quote).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&quote_for_display(&quote))?
    );

    if quote.executable {
        info!(
            market = %market,
            token_id = %token_id,
            slippage_bps = quote.slippage_bps,
            protocol_tax_bps = quote.protocol_tax_bps,
            "sell quote executable"
        );
    } else {
        warn!(
            market = %market,
            token_id = %token_id,
            slippage_bps = quote.slippage_bps,
            reason = %quote.reason,
            "sell quote blocked"
        );
    }

    Ok(())
}

async fn read_ot_decimals(settings: &Settings, market: Address, token_id: U256) -> Result<u8> {
    use crate::abi::FTMarket;
    let provider = ProviderBuilder::new().connect_http(settings.rpc.http_url.parse::<Url>()?);
    let rpc_limiter = RpcRateLimiter::new(settings.rpc.max_requests_per_second);
    let market_contract = FTMarket::new(market, provider);
    rpc_limiter.wait().await;
    Ok(market_contract.decimals(token_id).call().await?)
}

async fn validate_pricing(settings: Settings, args: ValidatePricingArgs) -> Result<()> {
    settings.validate()?;
    let tolerance_wei =
        U256::from_str_radix(&args.tolerance_wei, 10).wrap_err("failed to parse tolerance-wei")?;
    let quote_provider = ProviderBuilder::new().connect_http(settings.rpc.http_url.parse::<Url>()?);
    let scan_url = args
        .scan_rpc_url
        .as_deref()
        .unwrap_or(&settings.rpc.http_url);
    let scan_provider = ProviderBuilder::new().connect_http(scan_url.parse::<Url>()?);
    let quote_limiter = RpcRateLimiter::new(settings.rpc.max_requests_per_second);
    let scan_limiter = RpcRateLimiter::new(args.scan_max_requests_per_second);
    let summary = validation::validate_pricing(
        quote_provider,
        scan_provider,
        settings,
        quote_limiter,
        scan_limiter,
        validation::ValidationOptions {
            samples: args.samples,
            lookback_blocks: args.lookback_blocks,
            chunk_size: args.chunk_size,
            tolerance_wei,
            output_path: args.output_path,
            market_limit: args.market_limit,
            market_offset: args.market_offset,
            market_batch_size: args.market_batch_size,
            market_order: args.market_order,
            market_status: args.market_status,
        },
    )
    .await?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "requested_samples": summary.requested_samples,
            "collected_candidates": summary.collected_candidates,
            "validated": summary.validated,
            "passed": summary.passed,
            "failed": summary.failed,
            "skipped": summary.skipped,
            "max_abs_user_diff": summary.max_abs_user_diff.to_string(),
            "max_abs_treasury_diff": summary.max_abs_treasury_diff.to_string(),
            "elapsed_ms": summary.elapsed_ms,
            "output_path": summary.output_path,
        }))?
    );

    if summary.failed > 0 {
        return Err(eyre!(
            "pricing validation found {} failed samples",
            summary.failed
        ));
    }
    if summary.validated < args.samples {
        warn!(
            requested = args.samples,
            validated = summary.validated,
            skipped = summary.skipped,
            "pricing validation completed with fewer validated samples than requested"
        );
    }

    Ok(())
}

async fn validate_pricing_log(args: ValidatePricingLogArgs) -> Result<()> {
    let tolerance_wei =
        U256::from_str_radix(&args.tolerance_wei, 10).wrap_err("failed to parse tolerance-wei")?;
    let summary = validation::validate_pricing_logs(&args.input, tolerance_wei).await?;

    println!("{}", serde_json::to_string_pretty(&summary)?);

    if summary.failed > 0 {
        return Err(eyre!(
            "pricing log validation found {} failed samples",
            summary.failed
        ));
    }

    Ok(())
}

fn quote_for_display(quote: &price::SellQuote) -> serde_json::Value {
    serde_json::json!({
        "market": quote.market.to_string(),
        "token_id": quote.token_id.to_string(),
        "ot_decimals": quote.ot_decimals,
        "ot_amount": quote.ot_amount.to_string(),
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
    })
}

fn signer_from_env(settings: &Settings) -> Result<PrivateKeySigner> {
    let key = std::env::var(&settings.wallet.private_key_env)
        .with_context(|| format!("{} is not set", settings.wallet.private_key_env))?;
    key.parse::<PrivateKeySigner>()
        .wrap_err("failed to parse private key")
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "forty_two_sniper=info,info".into());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
