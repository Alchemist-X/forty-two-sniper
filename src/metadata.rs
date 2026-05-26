use std::time::Duration;

use alloy::primitives::Address;
use eyre::Result;
use reqwest::Url;
use serde_json::Value;

use crate::config::MetadataConfig;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    config: MetadataConfig,
}

impl Client {
    pub fn new(config: MetadataConfig) -> Self {
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(2)
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .expect("metadata client must build");

        Self { http, config }
    }

    pub async fn market(&self, market: Address) -> Result<Value> {
        let base = self.config.rest_base_url.trim_end_matches('/');
        let url = Url::parse(&format!("{base}/markets/{market:#x}"))?;
        let value = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(value)
    }
}
