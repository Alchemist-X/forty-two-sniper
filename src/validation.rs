use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use alloy::{
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::{BlockNumberOrTag, Filter, Log},
    sol_types::SolEvent,
};
use eyre::{Result, eyre};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use crate::{
    abi::FTMarket,
    config::Settings,
    price::{PriceEngine, SellAmount, append_jsonl},
    rate_limit::RpcRateLimiter,
};

#[derive(Debug, Clone)]
pub struct ValidationOptions {
    pub samples: usize,
    pub lookback_blocks: u64,
    pub chunk_size: u64,
    pub tolerance_wei: U256,
    pub output_path: String,
    pub market_limit: usize,
    pub market_offset: usize,
    pub market_batch_size: usize,
    pub market_order: String,
    pub market_status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationSummary {
    pub requested_samples: usize,
    pub collected_candidates: usize,
    pub validated: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub max_abs_user_diff: U256,
    pub max_abs_treasury_diff: U256,
    pub elapsed_ms: u128,
    pub output_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PricingLogSummary {
    pub inputs: Vec<String>,
    pub unique_samples: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub unique_markets: usize,
    pub unique_token_ids: usize,
    pub block_min: Option<u64>,
    pub block_max: Option<u64>,
    pub zero_diff_samples: usize,
    pub tolerance_wei: String,
    pub max_abs_user_diff: String,
    pub max_abs_treasury_diff: String,
    pub protocol_tax_bps_min: Option<String>,
    pub protocol_tax_bps_max: Option<String>,
    pub slippage_auditable_samples: usize,
    pub slippage_formula_note: String,
}

#[derive(Debug, Clone)]
struct RedeemCase {
    market: Address,
    block_number: u64,
    tx_hash: B256,
    log_index: Option<u64>,
    token_id: U256,
    collateral_to_user: U256,
    ot_to_pool: U256,
    collateral_to_treasury: U256,
}

#[derive(Debug, Deserialize)]
struct MarketsResponse {
    data: Vec<MarketSummary>,
}

#[derive(Debug, Deserialize)]
struct MarketSummary {
    address: Address,
}

#[derive(Debug, Clone)]
struct ValidationResult {
    case: RedeemCase,
    status: ValidationStatus,
}

#[derive(Debug, Clone)]
enum ValidationStatus {
    Passed {
        computed_user: U256,
        computed_treasury: U256,
        user_diff: U256,
        treasury_diff: U256,
    },
    Failed {
        computed_user: U256,
        computed_treasury: U256,
        user_diff: U256,
        treasury_diff: U256,
    },
    Skipped {
        reason: String,
    },
}

pub async fn validate_pricing<QuoteP, ScanP>(
    quote_provider: QuoteP,
    scan_provider: ScanP,
    settings: Settings,
    quote_limiter: RpcRateLimiter,
    scan_limiter: RpcRateLimiter,
    options: ValidationOptions,
) -> Result<ValidationSummary>
where
    QuoteP: Provider + Clone,
    ScanP: Provider + Clone,
{
    if options.samples == 0 {
        return Err(eyre!("samples must be greater than zero"));
    }
    if options.chunk_size == 0 {
        return Err(eyre!("chunk_size must be greater than zero"));
    }
    if options.market_limit == 0 {
        return Err(eyre!("market_limit must be greater than zero"));
    }
    if options.market_batch_size == 0 {
        return Err(eyre!("market_batch_size must be greater than zero"));
    }

    let started = Instant::now();
    let markets = fetch_market_addresses(
        &settings.metadata.rest_base_url,
        options.market_limit,
        options.market_offset,
        &options.market_order,
        options.market_status.as_deref(),
    )
    .await?;
    if markets.is_empty() {
        return Err(eyre!("no 42Space markets returned by REST API"));
    }

    scan_limiter.wait().await;
    let latest = scan_provider.get_block_number().await?;
    let from_limit = latest.saturating_sub(options.lookback_blocks);
    let mut cursor_to = latest;
    let mut candidates = Vec::new();
    let candidate_target = options
        .samples
        .saturating_add(options.samples.saturating_div(5))
        .max(options.samples);

    while candidates.len() < candidate_target && cursor_to > from_limit {
        let from = cursor_to
            .saturating_sub(options.chunk_size - 1)
            .max(from_limit);

        for market_batch in markets.chunks(options.market_batch_size) {
            if candidates.len() >= candidate_target {
                break;
            }

            let logs =
                get_redeem_logs(&scan_provider, &scan_limiter, market_batch, from, cursor_to)
                    .await?;
            info!(
                from,
                to = cursor_to,
                market_count = market_batch.len(),
                logs = logs.len(),
                "scanned historical RedeemSwap range"
            );

            for log in logs.into_iter().rev() {
                let Ok(decoded) = log.log_decode::<FTMarket::RedeemSwap>() else {
                    continue;
                };
                let Some(block_number) = log.block_number else {
                    continue;
                };
                let Some(tx_hash) = log.transaction_hash else {
                    continue;
                };
                let data = decoded.inner.data;
                candidates.push(RedeemCase {
                    market: log.address(),
                    block_number,
                    tx_hash,
                    log_index: log.log_index,
                    token_id: data.tokenId,
                    collateral_to_user: data.collateralToUser,
                    ot_to_pool: data.otToPool,
                    collateral_to_treasury: data.collateralToTreasury,
                });
            }
        }

        if from == 0 {
            break;
        }
        cursor_to = from - 1;
    }

    let mut unique_candidates = dedupe_market_block(candidates);
    unique_candidates.truncate(options.samples.saturating_mul(2));
    let collected_candidates = unique_candidates.len();
    let engine = PriceEngine::new(quote_provider.clone(), settings, quote_limiter.clone());
    let mut results = Vec::new();

    for case in unique_candidates {
        let compared = results
            .iter()
            .filter(|result: &&ValidationResult| {
                matches!(
                    result.status,
                    ValidationStatus::Passed { .. } | ValidationStatus::Failed { .. }
                )
            })
            .count();
        if compared >= options.samples {
            break;
        }

        match validate_case(
            &quote_provider,
            &engine,
            &quote_limiter,
            case,
            options.tolerance_wei,
            &options.output_path,
        )
        .await
        {
            Ok(result) => results.push(result),
            Err(err) => warn!(?err, "validation case failed before comparison"),
        }
    }

    let mut summary = ValidationSummary {
        requested_samples: options.samples,
        collected_candidates,
        validated: 0,
        passed: 0,
        failed: 0,
        skipped: 0,
        max_abs_user_diff: U256::ZERO,
        max_abs_treasury_diff: U256::ZERO,
        elapsed_ms: started.elapsed().as_millis(),
        output_path: options.output_path,
    };

    for result in &results {
        match &result.status {
            ValidationStatus::Passed {
                user_diff,
                treasury_diff,
                ..
            } => {
                summary.validated += 1;
                summary.passed += 1;
                summary.max_abs_user_diff = summary.max_abs_user_diff.max(*user_diff);
                summary.max_abs_treasury_diff = summary.max_abs_treasury_diff.max(*treasury_diff);
            }
            ValidationStatus::Failed {
                user_diff,
                treasury_diff,
                ..
            } => {
                summary.validated += 1;
                summary.failed += 1;
                summary.max_abs_user_diff = summary.max_abs_user_diff.max(*user_diff);
                summary.max_abs_treasury_diff = summary.max_abs_treasury_diff.max(*treasury_diff);
            }
            ValidationStatus::Skipped { .. } => {
                summary.skipped += 1;
            }
        }
    }

    append_jsonl(
        &summary.output_path,
        &json!({
            "type": "summary",
            "requested_samples": summary.requested_samples,
            "collected_candidates": summary.collected_candidates,
            "validated": summary.validated,
            "passed": summary.passed,
            "failed": summary.failed,
            "skipped": summary.skipped,
            "max_abs_user_diff": summary.max_abs_user_diff.to_string(),
            "max_abs_treasury_diff": summary.max_abs_treasury_diff.to_string(),
            "elapsed_ms": summary.elapsed_ms,
        }),
    )
    .await?;

    Ok(summary)
}

pub async fn validate_pricing_logs(
    paths: &[PathBuf],
    tolerance_wei: U256,
) -> Result<PricingLogSummary> {
    let mut seen = HashSet::<String>::new();
    let mut markets = HashSet::<String>::new();
    let mut token_ids = HashSet::<String>::new();
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut zero_diff_samples = 0usize;
    let mut block_min: Option<u64> = None;
    let mut block_max: Option<u64> = None;
    let mut max_abs_user_diff = U256::ZERO;
    let mut max_abs_treasury_diff = U256::ZERO;
    let mut tax_min: Option<U256> = None;
    let mut tax_max: Option<U256> = None;
    let mut slippage_auditable_samples = 0usize;

    for path in paths {
        let raw = tokio::fs::read_to_string(path).await?;
        for (line_index, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line).map_err(|err| {
                eyre!("{}:{} invalid JSONL: {err}", path.display(), line_index + 1)
            })?;
            if value.get("type").and_then(serde_json::Value::as_str) != Some("sample") {
                continue;
            }

            let tx_hash = string_field(path, line_index, &value, "tx_hash")?;
            let log_index = value
                .get("log_index")
                .map(serde_json::Value::to_string)
                .unwrap_or_else(|| "null".to_owned());
            if !seen.insert(format!("{tx_hash}:{log_index}")) {
                continue;
            }

            if let Some(market) = value.get("market").and_then(serde_json::Value::as_str) {
                markets.insert(market.to_owned());
            }
            if let Some(token_id) = value.get("token_id").and_then(serde_json::Value::as_str) {
                token_ids.insert(token_id.to_owned());
            }
            if let Some(block_number) = value
                .get("block_number")
                .and_then(serde_json::Value::as_u64)
            {
                block_min =
                    Some(block_min.map_or(block_number, |current| current.min(block_number)));
                block_max =
                    Some(block_max.map_or(block_number, |current| current.max(block_number)));
            }

            if value.get("status").and_then(serde_json::Value::as_str) == Some("skipped") {
                skipped += 1;
                continue;
            }

            let event_user = u256_field(path, line_index, &value, "event_collateral_to_user")?;
            let event_treasury =
                u256_field(path, line_index, &value, "event_collateral_to_treasury")?;
            let computed_user =
                u256_field(path, line_index, &value, "computed_collateral_to_user")?;
            let computed_treasury =
                u256_field(path, line_index, &value, "computed_collateral_to_treasury")?;
            let user_diff = value
                .get("user_diff")
                .and_then(serde_json::Value::as_str)
                .map(|raw| parse_u256_field(path, line_index, "user_diff", raw))
                .transpose()?
                .unwrap_or_else(|| abs_diff(computed_user, event_user));
            let treasury_diff = value
                .get("treasury_diff")
                .and_then(serde_json::Value::as_str)
                .map(|raw| parse_u256_field(path, line_index, "treasury_diff", raw))
                .transpose()?
                .unwrap_or_else(|| abs_diff(computed_treasury, event_treasury));

            max_abs_user_diff = max_abs_user_diff.max(user_diff);
            max_abs_treasury_diff = max_abs_treasury_diff.max(treasury_diff);
            if user_diff.is_zero() && treasury_diff.is_zero() {
                zero_diff_samples += 1;
            }

            let gross = event_user + event_treasury;
            if !gross.is_zero() {
                let tax = scaled_bps(event_treasury, gross);
                tax_min = Some(tax_min.map_or(tax, |current| current.min(tax)));
                tax_max = Some(tax_max.map_or(tax, |current| current.max(tax)));
            }

            if value.get("spot_collateral_value").is_some() && value.get("slippage_bps").is_some() {
                slippage_auditable_samples += 1;
            }

            if user_diff <= tolerance_wei
                && treasury_diff <= tolerance_wei
                && value.get("status").and_then(serde_json::Value::as_str) == Some("passed")
            {
                passed += 1;
            } else {
                failed += 1;
            }
        }
    }

    Ok(PricingLogSummary {
        inputs: paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        unique_samples: seen.len(),
        passed,
        failed,
        skipped,
        unique_markets: markets.len(),
        unique_token_ids: token_ids.len(),
        block_min,
        block_max,
        zero_diff_samples,
        tolerance_wei: tolerance_wei.to_string(),
        max_abs_user_diff: max_abs_user_diff.to_string(),
        max_abs_treasury_diff: max_abs_treasury_diff.to_string(),
        protocol_tax_bps_min: tax_min.map(format_scaled_bps),
        protocol_tax_bps_max: tax_max.map(format_scaled_bps),
        slippage_auditable_samples,
        slippage_formula_note: if slippage_auditable_samples == 0 {
            "existing validation logs do not include spot_collateral_value; slippage formula is covered by unit tests and quote logs".to_owned()
        } else {
            "slippage fields were present in at least one log sample".to_owned()
        },
    })
}

async fn fetch_market_addresses(
    rest_base_url: &str,
    limit: usize,
    offset: usize,
    order: &str,
    status: Option<&str>,
) -> Result<Vec<Address>> {
    let base = rest_base_url.trim_end_matches('/');
    let mut url = Url::parse(&format!("{base}/markets"))?;
    url.query_pairs_mut()
        .append_pair("limit", &limit.to_string())
        .append_pair("offset", &offset.to_string())
        .append_pair("order", order);
    if let Some(status) = status
        && !status.trim().is_empty()
    {
        url.query_pairs_mut().append_pair("status", status);
    }

    let response: MarketsResponse = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(response
        .data
        .into_iter()
        .map(|market| market.address)
        .collect())
}

async fn get_redeem_logs<P>(
    provider: &P,
    rpc_limiter: &RpcRateLimiter,
    markets: &[Address],
    from: u64,
    to: u64,
) -> Result<Vec<Log>>
where
    P: Provider + Clone,
{
    let filter = redeem_filter(markets.to_vec(), from, to);
    match get_logs_with_retry(provider, rpc_limiter, &filter).await {
        Ok(logs) => Ok(logs),
        Err(err) if markets.len() > 1 && !is_rate_limit_error(&err) => {
            warn!(
                ?err,
                market_count = markets.len(),
                "batched log scan failed; retrying one market at a time"
            );
            let mut logs = Vec::new();
            for market in markets {
                let filter = redeem_filter(vec![*market], from, to);
                logs.extend(get_logs_with_retry(provider, rpc_limiter, &filter).await?);
            }
            Ok(logs)
        }
        Err(err) => Err(err),
    }
}

fn redeem_filter(markets: Vec<Address>, from: u64, to: u64) -> Filter {
    Filter::new()
        .address(markets)
        .event_signature(FTMarket::RedeemSwap::SIGNATURE_HASH)
        .from_block(BlockNumberOrTag::Number(from))
        .to_block(BlockNumberOrTag::Number(to))
}

async fn get_logs_with_retry<P>(
    provider: &P,
    rpc_limiter: &RpcRateLimiter,
    filter: &Filter,
) -> Result<Vec<Log>>
where
    P: Provider + Clone,
{
    let mut delay = Duration::from_secs(2);
    for attempt in 0..=4 {
        rpc_limiter.wait().await;
        match provider.get_logs(filter).await {
            Ok(logs) => return Ok(logs),
            Err(err) if is_rate_limit_text(&err.to_string()) && attempt < 4 => {
                warn!(
                    attempt = attempt + 1,
                    sleep_ms = delay.as_millis(),
                    "log scan hit RPC rate limit; backing off"
                );
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
            Err(err) => return Err(err.into()),
        }
    }

    Err(eyre!("log scan retry loop exited unexpectedly"))
}

fn is_rate_limit_error(err: &eyre::Report) -> bool {
    is_rate_limit_text(&err.to_string())
}

fn is_rate_limit_text(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("429")
        || lower.contains("too many")
        || lower.contains("rate limit")
        || lower.contains("usage limit")
}

async fn validate_case<P>(
    provider: &P,
    engine: &PriceEngine<P>,
    rpc_limiter: &RpcRateLimiter,
    case: RedeemCase,
    tolerance_wei: U256,
    output_path: &str,
) -> Result<ValidationResult>
where
    P: Provider + Clone,
{
    if case.block_number == 0 {
        let result = ValidationResult {
            case,
            status: ValidationStatus::Skipped {
                reason: "block 0 has no previous state".to_owned(),
            },
        };
        write_result(output_path, &result).await?;
        return Ok(result);
    }

    let swap_count =
        count_market_swaps_in_block(provider, rpc_limiter, case.market, case.block_number).await?;
    if swap_count != 1 {
        let result = ValidationResult {
            case,
            status: ValidationStatus::Skipped {
                reason: format!(
                    "market has {swap_count} MintSwap/RedeemSwap logs in block; block-1 replay is ambiguous"
                ),
            },
        };
        write_result(output_path, &result).await?;
        return Ok(result);
    }

    let replay_block = case.block_number - 1;
    let quote = engine
        .quote_sell_exact_ot_at_block(
            case.market,
            case.token_id,
            SellAmount::Exact(case.ot_to_pool),
            Some(replay_block),
        )
        .await?;

    let user_diff = abs_diff(quote.collateral_to_user, case.collateral_to_user);
    let treasury_diff = abs_diff(quote.collateral_to_treasury, case.collateral_to_treasury);
    let passed = user_diff <= tolerance_wei && treasury_diff <= tolerance_wei;

    let result = ValidationResult {
        case,
        status: if passed {
            ValidationStatus::Passed {
                computed_user: quote.collateral_to_user,
                computed_treasury: quote.collateral_to_treasury,
                user_diff,
                treasury_diff,
            }
        } else {
            ValidationStatus::Failed {
                computed_user: quote.collateral_to_user,
                computed_treasury: quote.collateral_to_treasury,
                user_diff,
                treasury_diff,
            }
        },
    };
    write_result(output_path, &result).await?;
    Ok(result)
}

async fn count_market_swaps_in_block<P>(
    provider: &P,
    rpc_limiter: &RpcRateLimiter,
    market: Address,
    block_number: u64,
) -> Result<usize>
where
    P: Provider + Clone,
{
    let mut count = 0usize;
    for topic in [
        FTMarket::MintSwap::SIGNATURE_HASH,
        FTMarket::RedeemSwap::SIGNATURE_HASH,
    ] {
        let filter = Filter::new()
            .address(market)
            .event_signature(topic)
            .from_block(BlockNumberOrTag::Number(block_number))
            .to_block(BlockNumberOrTag::Number(block_number));
        rpc_limiter.wait().await;
        count += provider.get_logs(&filter).await?.len();
    }
    Ok(count)
}

fn dedupe_market_block(cases: Vec<RedeemCase>) -> Vec<RedeemCase> {
    let mut counts = HashMap::<(u64, Address), usize>::new();
    for case in &cases {
        *counts.entry((case.block_number, case.market)).or_default() += 1;
    }

    cases
        .into_iter()
        .filter(|case| counts[&(case.block_number, case.market)] == 1)
        .collect()
}

async fn write_result(path: &str, result: &ValidationResult) -> Result<()> {
    let mut record = serde_json::Map::new();
    record.insert("type".to_owned(), json!("sample"));
    record.insert("market".to_owned(), json!(result.case.market.to_string()));
    record.insert("block_number".to_owned(), json!(result.case.block_number));
    record.insert(
        "replay_block".to_owned(),
        json!(result.case.block_number.saturating_sub(1)),
    );
    record.insert("tx_hash".to_owned(), json!(result.case.tx_hash.to_string()));
    record.insert("log_index".to_owned(), json!(result.case.log_index));
    record.insert(
        "token_id".to_owned(),
        json!(result.case.token_id.to_string()),
    );
    record.insert(
        "event_collateral_to_user".to_owned(),
        json!(result.case.collateral_to_user.to_string()),
    );
    record.insert(
        "event_ot_to_pool".to_owned(),
        json!(result.case.ot_to_pool.to_string()),
    );
    record.insert(
        "event_collateral_to_treasury".to_owned(),
        json!(result.case.collateral_to_treasury.to_string()),
    );

    match &result.status {
        ValidationStatus::Passed {
            computed_user,
            computed_treasury,
            user_diff,
            treasury_diff,
        } => {
            record.insert("status".to_owned(), json!("passed"));
            insert_comparison_fields(
                &mut record,
                *computed_user,
                *computed_treasury,
                *user_diff,
                *treasury_diff,
            );
        }
        ValidationStatus::Failed {
            computed_user,
            computed_treasury,
            user_diff,
            treasury_diff,
        } => {
            record.insert("status".to_owned(), json!("failed"));
            insert_comparison_fields(
                &mut record,
                *computed_user,
                *computed_treasury,
                *user_diff,
                *treasury_diff,
            );
        }
        ValidationStatus::Skipped { reason } => {
            record.insert("status".to_owned(), json!("skipped"));
            record.insert("reason".to_owned(), json!(reason));
        }
    }

    append_jsonl(path, &serde_json::Value::Object(record)).await
}

fn insert_comparison_fields(
    record: &mut serde_json::Map<String, serde_json::Value>,
    computed_user: U256,
    computed_treasury: U256,
    user_diff: U256,
    treasury_diff: U256,
) {
    record.insert(
        "computed_collateral_to_user".to_owned(),
        json!(computed_user.to_string()),
    );
    record.insert(
        "computed_collateral_to_treasury".to_owned(),
        json!(computed_treasury.to_string()),
    );
    record.insert("user_diff".to_owned(), json!(user_diff.to_string()));
    record.insert("treasury_diff".to_owned(), json!(treasury_diff.to_string()));
}

fn abs_diff(a: U256, b: U256) -> U256 {
    if a >= b { a - b } else { b - a }
}

fn string_field(
    path: &Path,
    line_index: usize,
    value: &serde_json::Value,
    field: &str,
) -> Result<String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| eyre!("{}:{} missing {field}", path.display(), line_index + 1))
}

fn u256_field(
    path: &Path,
    line_index: usize,
    value: &serde_json::Value,
    field: &str,
) -> Result<U256> {
    let raw = string_field(path, line_index, value, field)?;
    parse_u256_field(path, line_index, field, &raw)
}

fn parse_u256_field(path: &Path, line_index: usize, field: &str, raw: &str) -> Result<U256> {
    U256::from_str_radix(raw, 10).map_err(|err| {
        eyre!(
            "{}:{} invalid {field}: {err}",
            path.display(),
            line_index + 1
        )
    })
}

fn scaled_bps(numerator: U256, denominator: U256) -> U256 {
    if denominator.is_zero() {
        return U256::ZERO;
    }
    numerator * U256::from(10_000_000_000u64) / denominator
}

fn format_scaled_bps(value: U256) -> String {
    let scale = U256::from(1_000_000u64);
    let whole = value / scale;
    let fraction = value % scale;
    format!("{whole}.{:06}", fraction.to::<u64>())
}
