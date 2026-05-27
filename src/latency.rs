use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::primitives::Address;
use eyre::Result;
use serde_json::{Value, json};
use tokio::{fs::OpenOptions, io::AsyncWriteExt};
use tracing::debug;

use crate::config::Settings;

#[derive(Clone, Debug)]
pub struct LatencyLogger {
    enabled: bool,
    log_path: String,
    provider_label: String,
}

impl LatencyLogger {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            enabled: settings.latency.enabled,
            log_path: settings.latency.log_path.clone(),
            provider_label: settings.latency.provider_label.clone(),
        }
    }

    pub fn record(&self, stage: &str, elapsed_ms: u128, market: Option<Address>, extra: Value) {
        if !self.enabled {
            return;
        }

        let mut record = json!({
            "ts_ms": now_ms(),
            "provider": self.provider_label,
            "stage": stage,
            "elapsed_ms": elapsed_ms,
        });

        if let Some(market) = market {
            record["market"] = json!(market.to_string());
        }
        merge_extra(&mut record, extra);

        let path = self.log_path.clone();
        tokio::spawn(async move {
            if let Err(err) = append_jsonl(&path, &record).await {
                debug!(?err, "failed to write latency log");
            }
        });
    }

    pub async fn record_blocking(
        &self,
        stage: &str,
        elapsed_ms: u128,
        market: Option<Address>,
        extra: Value,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let mut record = json!({
            "ts_ms": now_ms(),
            "provider": self.provider_label,
            "stage": stage,
            "elapsed_ms": elapsed_ms,
        });

        if let Some(market) = market {
            record["market"] = json!(market.to_string());
        }
        merge_extra(&mut record, extra);
        append_jsonl(&self.log_path, &record).await
    }
}

pub async fn append_jsonl(path: &str, record: &Value) -> Result<()> {
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

fn merge_extra(record: &mut Value, extra: Value) {
    let Some(record_object) = record.as_object_mut() else {
        return;
    };
    let Value::Object(extra_object) = extra else {
        return;
    };

    for (key, value) in extra_object {
        record_object.insert(key, value);
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
