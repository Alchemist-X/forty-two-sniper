mod abi;
mod config;
mod execution;
mod metadata;
mod rate_limit;

use std::{collections::HashSet, path::PathBuf, time::Instant};

use abi::FTMarketController;
use alloy::{
    primitives::Address,
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::{BlockNumberOrTag, Filter},
    signers::local::PrivateKeySigner,
    sol_types::SolEvent,
};
use clap::{Parser, Subcommand};
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
