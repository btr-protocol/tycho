//! Log sources for enumerating a protocol's components.

use alloy::{
    primitives::{Address, Bytes, B256},
    rpc::types::Filter,
    transports::http::reqwest,
};
use async_trait::async_trait;
use futures03::{stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tycho_ethereum::rpc::EthereumRpcClient;

use crate::extractor::ExtractionError;

/// An event log with the fields snapshot sources need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLog {
    pub block_number: u64,
    pub log_index: u64,
    pub transaction_hash: B256,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

/// Fetches historical event logs.
#[async_trait]
pub trait LogSource: Send + Sync {
    /// Returns the logs `address` emitted with first topic `topic0` in blocks `from..=to`, in
    /// chain order.
    async fn logs(
        &self,
        address: Address,
        topic0: B256,
        from: u64,
        to: u64,
    ) -> Result<Vec<EventLog>, ExtractionError>;
}

/// Reads logs with `eth_getLogs`, splitting the range into requests of `block_range` blocks.
pub struct RpcLogSource {
    client: EthereumRpcClient,
    block_range: u64,
    concurrency: usize,
}

impl RpcLogSource {
    pub fn new(client: EthereumRpcClient, block_range: u64, concurrency: usize) -> Self {
        Self { client, block_range: block_range.max(1), concurrency: concurrency.max(1) }
    }
}

#[async_trait]
impl LogSource for RpcLogSource {
    async fn logs(
        &self,
        address: Address,
        topic0: B256,
        from: u64,
        to: u64,
    ) -> Result<Vec<EventLog>, ExtractionError> {
        let ranges = (from..=to)
            .step_by(self.block_range as usize)
            .map(|start| (start, to.min(start + self.block_range - 1)));
        let chunks: Vec<Vec<EventLog>> = stream::iter(ranges)
            .map(|(start, end)| async move {
                let filter = Filter::new()
                    .address(address)
                    .event_signature(topic0)
                    .from_block(start)
                    .to_block(end);
                let logs = self
                    .client
                    .get_logs(&filter)
                    .await
                    .map_err(|e| ExtractionError::Setup(e.to_string()))?;
                logs.into_iter()
                    .map(|log| {
                        Ok::<_, ExtractionError>(EventLog {
                            block_number: log.block_number.ok_or_else(|| {
                                ExtractionError::Setup("eth_getLogs returned a pending log".into())
                            })?,
                            log_index: log.log_index.unwrap_or_default(),
                            transaction_hash: log.transaction_hash.unwrap_or_default(),
                            topics: log.topics().to_vec(),
                            data: log.data().data.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .buffered(self.concurrency)
            .try_collect()
            .await?;
        Ok(chunks.into_iter().flatten().collect())
    }
}

/// Reads logs from an Envio HyperSync endpoint through its JSON query API.
pub struct HyperSyncLogSource {
    client: reqwest::Client,
    url: String,
    api_key: String,
}

impl HyperSyncLogSource {
    pub fn new(url: &str, api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: format!("{}/query", url.trim_end_matches('/')),
            api_key,
        }
    }
}

#[derive(Serialize)]
struct HyperSyncQuery {
    from_block: u64,
    /// Exclusive.
    to_block: u64,
    logs: Vec<HyperSyncLogSelection>,
    field_selection: HyperSyncFieldSelection,
}

#[derive(Serialize)]
struct HyperSyncLogSelection {
    address: Vec<Address>,
    topics: Vec<Vec<B256>>,
}

#[derive(Serialize)]
struct HyperSyncFieldSelection {
    log: Vec<&'static str>,
}

#[derive(Deserialize)]
struct HyperSyncResponse {
    data: Vec<HyperSyncData>,
    next_block: u64,
}

#[derive(Deserialize)]
struct HyperSyncData {
    #[serde(default)]
    logs: Vec<HyperSyncLog>,
}

#[derive(Deserialize)]
struct HyperSyncLog {
    block_number: u64,
    log_index: u64,
    transaction_hash: B256,
    data: Bytes,
    topic0: Option<B256>,
    topic1: Option<B256>,
    topic2: Option<B256>,
    topic3: Option<B256>,
}

impl From<HyperSyncLog> for EventLog {
    fn from(log: HyperSyncLog) -> Self {
        Self {
            block_number: log.block_number,
            log_index: log.log_index,
            transaction_hash: log.transaction_hash,
            topics: [log.topic0, log.topic1, log.topic2, log.topic3]
                .into_iter()
                .flatten()
                .collect(),
            data: log.data,
        }
    }
}

#[async_trait]
impl LogSource for HyperSyncLogSource {
    async fn logs(
        &self,
        address: Address,
        topic0: B256,
        from: u64,
        to: u64,
    ) -> Result<Vec<EventLog>, ExtractionError> {
        let mut logs = Vec::new();
        let mut next = from;
        while next <= to {
            let query = HyperSyncQuery {
                from_block: next,
                to_block: to + 1,
                logs: vec![HyperSyncLogSelection {
                    address: vec![address],
                    topics: vec![vec![topic0]],
                }],
                field_selection: HyperSyncFieldSelection {
                    log: vec![
                        "block_number",
                        "log_index",
                        "transaction_hash",
                        "data",
                        "topic0",
                        "topic1",
                        "topic2",
                        "topic3",
                    ],
                },
            };
            let response = self
                .client
                .post(&self.url)
                .bearer_auth(&self.api_key)
                .json(&query)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(|e| ExtractionError::Setup(format!("HyperSync query failed: {e}")))?
                .json::<HyperSyncResponse>()
                .await
                .map_err(|e| ExtractionError::Setup(format!("HyperSync response invalid: {e}")))?;
            if response.next_block <= next {
                return Err(ExtractionError::Setup(format!(
                    "HyperSync made no progress past block {next}"
                )));
            }
            logs.extend(
                response
                    .data
                    .into_iter()
                    .flat_map(|data| data.logs)
                    .map(EventLog::from),
            );
            next = response.next_block;
        }
        logs.sort_by_key(|log| (log.block_number, log.log_index));
        Ok(logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hypersync_response_parses_into_event_logs() {
        let body = r#"{
            "data": [{"logs": [{
                "block_number": 7, "log_index": 3,
                "transaction_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
                "data": "0x0102",
                "topic0": "0x2222222222222222222222222222222222222222222222222222222222222222",
                "topic1": "0x3333333333333333333333333333333333333333333333333333333333333333",
                "topic2": null, "topic3": null
            }]}, {"blocks": []}],
            "archive_height": 100, "next_block": 50, "total_execution_time": 1
        }"#;
        let response: HyperSyncResponse = serde_json::from_str(body).unwrap();
        assert_eq!(response.next_block, 50);
        let logs: Vec<EventLog> = response
            .data
            .into_iter()
            .flat_map(|d| d.logs)
            .map(EventLog::from)
            .collect();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].topics, vec![B256::repeat_byte(0x22), B256::repeat_byte(0x33)]);
        assert_eq!(logs[0].data, Bytes::from(vec![1, 2]));
    }
}
