use std::{fs, path::Path, str::FromStr};

use alloy::primitives::{Address, U256};
use eyre::{Result, eyre};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Settings {
    pub rpc: RpcConfig,
    pub wallet: WalletConfig,
    pub contracts: ContractConfig,
    pub strategy: StrategyConfig,
    #[serde(default)]
    pub metadata: MetadataConfig,
    #[serde(default)]
    pub filters: FilterConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcConfig {
    pub http_url: String,
    pub ws_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalletConfig {
    #[serde(default = "default_private_key_env")]
    pub private_key_env: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContractConfig {
    pub controller: String,
    pub router: String,
    pub collateral: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StrategyConfig {
    #[serde(default = "default_token_id")]
    pub outcome_token_id: String,
    #[serde(default = "default_buy_amount")]
    pub buy_amount_units: String,
    #[serde(default = "default_decimals")]
    pub collateral_decimals: u8,
    #[serde(default)]
    pub min_out_or_max_in: String,
    #[serde(default = "default_gas_limit")]
    pub gas_limit: u64,
    #[serde(default = "default_gas_bump_bps")]
    pub gas_price_bump_bps: u64,
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetadataConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_rest_base_url")]
    pub rest_base_url: String,
    #[serde(default = "default_metadata_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FilterConfig {
    #[serde(default)]
    pub allowed_collateral: Vec<String>,
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rest_base_url: default_rest_base_url(),
            timeout_ms: default_metadata_timeout_ms(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        let mut settings: Settings = toml::from_str(&raw)?;
        settings.apply_env_overrides();
        Ok(settings)
    }

    pub fn validate(&self) -> Result<()> {
        self.controller_address()?;
        self.router_address()?;
        self.collateral_address()?;
        self.outcome_token_id()?;
        self.buy_amount()?;
        self.min_out_or_max_in()?;

        if self.rpc.http_url.trim().is_empty() {
            return Err(eyre!("rpc.http_url must not be empty"));
        }
        if self.rpc.ws_url.trim().is_empty() {
            return Err(eyre!("rpc.ws_url must not be empty"));
        }
        if self.strategy.gas_limit == 0 {
            return Err(eyre!("strategy.gas_limit must be greater than zero"));
        }
        Ok(())
    }

    pub fn controller_address(&self) -> Result<Address> {
        parse_address("contracts.controller", &self.contracts.controller)
    }

    pub fn router_address(&self) -> Result<Address> {
        parse_address("contracts.router", &self.contracts.router)
    }

    pub fn collateral_address(&self) -> Result<Address> {
        parse_address("contracts.collateral", &self.contracts.collateral)
    }

    pub fn outcome_token_id(&self) -> Result<U256> {
        parse_u256("strategy.outcome_token_id", &self.strategy.outcome_token_id)
    }

    pub fn buy_amount(&self) -> Result<U256> {
        parse_token_units(
            "strategy.buy_amount_units",
            &self.strategy.buy_amount_units,
            self.strategy.collateral_decimals,
        )
    }

    pub fn min_out_or_max_in(&self) -> Result<U256> {
        parse_u256(
            "strategy.min_out_or_max_in",
            &self.strategy.min_out_or_max_in,
        )
    }

    pub fn is_allowed_collateral(&self, collateral: Address) -> bool {
        if self.filters.allowed_collateral.is_empty() {
            return collateral == self.collateral_address().unwrap_or_default();
        }

        self.filters.allowed_collateral.iter().any(|addr| {
            parse_address("filters.allowed_collateral", addr)
                .map(|allowed| allowed == collateral)
                .unwrap_or(false)
        })
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(value) = std::env::var("SNIPER_HTTP_RPC_URL") {
            self.rpc.http_url = value;
        }
        if let Ok(value) = std::env::var("SNIPER_WS_RPC_URL") {
            self.rpc.ws_url = value;
        }
        if let Ok(value) = std::env::var("SNIPER_BUY_AMOUNT_UNITS") {
            self.strategy.buy_amount_units = value;
        }
        if let Ok(value) = std::env::var("SNIPER_DRY_RUN") {
            self.strategy.dry_run = matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES");
        }
    }
}

fn parse_address(field: &str, value: &str) -> Result<Address> {
    Address::from_str(value).map_err(|err| eyre!("{field} is not a valid address: {err}"))
}

fn parse_u256(field: &str, value: &str) -> Result<U256> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(U256::ZERO);
    }

    U256::from_str_radix(trimmed, 10).map_err(|err| eyre!("{field} is not a valid uint256: {err}"))
}

fn parse_token_units(field: &str, value: &str, decimals: u8) -> Result<U256> {
    let value = value.trim();
    if value.is_empty() {
        return Err(eyre!("{field} must not be empty"));
    }
    if value.starts_with('-') {
        return Err(eyre!("{field} must not be negative"));
    }

    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return Err(eyre!("{field} has too many decimal points"));
    }
    if fraction.len() > decimals as usize {
        return Err(eyre!("{field} has more precision than collateral_decimals"));
    }

    let mut normalized = String::with_capacity(whole.len() + decimals as usize);
    normalized.push_str(if whole.is_empty() { "0" } else { whole });
    normalized.push_str(fraction);
    for _ in fraction.len()..decimals as usize {
        normalized.push('0');
    }

    parse_u256(field, &normalized)
}

fn default_private_key_env() -> String {
    "SNIPER_PRIVATE_KEY".to_owned()
}

fn default_token_id() -> String {
    "1".to_owned()
}

fn default_buy_amount() -> String {
    "10".to_owned()
}

fn default_decimals() -> u8 {
    18
}

fn default_gas_limit() -> u64 {
    300_000
}

fn default_gas_bump_bps() -> u64 {
    2_000
}

fn default_true() -> bool {
    true
}

fn default_rest_base_url() -> String {
    "https://rest.ft.42.space/api/v1".to_owned()
}

fn default_metadata_timeout_ms() -> u64 {
    250
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units_with_decimals() {
        let parsed = parse_token_units("x", "10.25", 18).unwrap();
        assert_eq!(parsed.to_string(), "10250000000000000000");
    }

    #[test]
    fn rejects_excess_precision() {
        let err = parse_token_units("x", "1.123", 2).unwrap_err();
        assert!(err.to_string().contains("more precision"));
    }
}
