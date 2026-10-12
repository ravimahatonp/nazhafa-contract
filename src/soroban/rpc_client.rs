use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur during RPC calls
#[derive(Debug, Error)]
pub enum RpcError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("RPC error (code {code}): {message}")]
    Rpc { code: i64, message: String },

    #[error("Failed to deserialize response: {0}")]
    Deserialize(String),
}

impl crate::utils::retry::IsRetryable for RpcError {
    fn is_retryable(&self) -> bool {
        match self {
            RpcError::Http(err) => {
                err.is_timeout()
                    || err.is_connect()
                    || err.status().is_some_and(|s| s.is_server_error() || s.as_u16() == 429)
            }
            RpcError::Rpc { code, .. } => *code == -32000 || *code == -32603 || *code == 429,
            RpcError::Deserialize(_) => false,
        }
    }
}

// ── JSON-RPC plumbing ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: serde_json::Value,
}

#[derive(Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

// ── Public result types ────────────────────────────────────────────────────────

/// Result returned by `simulateTransaction`.
#[derive(Debug, Deserialize)]
pub struct SimulationResult {
    /// Estimated resource cost.
    pub cost: Option<SimulationCost>,
    /// Encoded footprint XDR.
    pub footprint: Option<String>,
    /// Per-auth entries (may be empty).
    pub auth: Option<Vec<String>>,
    /// Error string, present when simulation failed.
    pub error: Option<String>,
    /// Latest ledger at time of simulation.
    #[serde(rename = "latestLedger")]
    pub latest_ledger: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SimulationCost {
    #[serde(rename = "cpuInsns")]
    pub cpu_insns: String,
    #[serde(rename = "memBytes")]
    pub mem_bytes: String,
}

/// Result returned by `sendTransaction`.
#[derive(Debug, Deserialize)]
pub struct SendResult {
    /// Transaction hash.
    pub hash: String,
    /// Immediate status after submission.
    pub status: TransactionStatus,
    /// Error XDR if submission was immediately rejected.
    #[serde(rename = "errorResultXdr")]
    pub error_result_xdr: Option<String>,
    /// Latest ledger seen by the node.
    #[serde(rename = "latestLedger")]
    pub latest_ledger: Option<String>,
    #[serde(rename = "latestLedgerCloseTime")]
    pub latest_ledger_close_time: Option<String>,
}

/// All statuses defined by the Soroban RPC spec.
#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionStatus {
    Pending,
    Success,
    Failed,
    NotFound,
    /// Catch-all for any future statuses.
    #[serde(other)]
    Unknown,
}

/// Full response from `getTransaction`.
#[derive(Debug, Clone, Deserialize)]
pub struct TransactionStatusResponse {
    pub status: TransactionStatus,
    /// Result XDR, present on SUCCESS.
    #[serde(rename = "resultXdr")]
    pub result_xdr: Option<String>,
    /// Result-meta XDR, present on SUCCESS.
    #[serde(rename = "resultMetaXdr")]
    pub result_meta_xdr: Option<String>,
    /// Envelope XDR, present on SUCCESS/FAILED.
    #[serde(rename = "envelopeXdr")]
    pub envelope_xdr: Option<String>,
    /// Ledger on which the tx was included.
    pub ledger: Option<u64>,
    #[serde(rename = "createdAt")]
    pub created_at: Option<String>,
    #[serde(rename = "latestLedger")]
    pub latest_ledger: Option<String>,
    #[serde(rename = "latestLedgerCloseTime")]
    pub latest_ledger_close_time: Option<String>,
}

// ── Trait abstraction ──────────────────────────────────────────────────────────

// Async abstraction over a Soroban JSON-RPC endpoint.
//
// The HTTP handlers depend on this trait so they can be unit-tested with a
// mock implementation instead of hitting the network.
#[async_trait::async_trait]
pub trait SorobanRpc: Send + Sync {
    /// Submit a signed transaction to the network.
    async fn send_transaction(&self, xdr: &str) -> Result<SendResult, RpcError>;

    /// Poll the status of a previously submitted transaction by its hash.
    async fn get_transaction_status(
        &self,
        hash: &str,
    ) -> Result<TransactionStatusResponse, RpcError>;
}

// ── Client ─────────────────────────────────────────────────────────────────────

/// A typed JSON-RPC client for a Soroban-enabled Horizon/RPC endpoint.
pub struct SorobanRpcClient {
    http: Client,
    endpoint: String,
    next_id: std::sync::atomic::AtomicU64,
}

impl SorobanRpcClient {
    /// Create a new client targeting `endpoint` (e.g. `"https://soroban-testnet.stellar.org"`).
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            endpoint: endpoint.into(),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    // ── Private helpers ────────────────────────────────────────────────────────

    fn next_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T, RpcError> {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: self.next_id(),
            method,
            params,
        };

        crate::utils::retry::retry_with_backoff(
            || async {
                let resp = self
                    .http
                    .post(&self.endpoint)
                    .json(&req)
                    .send()
                    .await?
                    .json::<JsonRpcResponse<T>>()
                    .await?;

                if let Some(err) = resp.error {
                    return Err(RpcError::Rpc {
                        code: err.code,
                        message: err.message,
                    });
                }

                resp.result.ok_or_else(|| {
                    RpcError::Deserialize("response contained neither result nor error".into())
                })
            },
            3,
            std::time::Duration::from_millis(200),
        )
        .await
    }

    // ── Public API ─────────────────────────────────────────────────────────────

    /// Simulate a transaction without submitting it.
    ///
    /// `xdr` is the base64-encoded TransactionEnvelope XDR.
    pub async fn simulate_transaction(&self, xdr: &str) -> Result<SimulationResult, RpcError> {
        let params = serde_json::json!({ "transaction": xdr });
        self.call::<SimulationResult>("simulateTransaction", params)
            .await
    }

    /// Submit a signed transaction to the network.
    ///
    /// `xdr` is the base64-encoded TransactionEnvelope XDR.
    pub async fn send_transaction(&self, xdr: &str) -> Result<SendResult, RpcError> {
        let params = serde_json::json!({ "transaction": xdr });
        self.call::<SendResult>("sendTransaction", params).await
    }

    /// Poll the status of a previously submitted transaction by its hash.
    pub async fn get_transaction_status(
        &self,
        hash: &str,
    ) -> Result<TransactionStatusResponse, RpcError> {
        let params = serde_json::json!({ "hash": hash });
        self.call::<TransactionStatusResponse>("getTransaction", params)
            .await
    }
}

#[async_trait::async_trait]
impl SorobanRpc for SorobanRpcClient {
    async fn send_transaction(&self, xdr: &str) -> Result<SendResult, RpcError> {
        // Delegate to the inherent method to avoid trait/inherent ambiguity.
        SorobanRpcClient::send_transaction(self, xdr).await
    }

    async fn get_transaction_status(
        &self,
        hash: &str,
    ) -> Result<TransactionStatusResponse, RpcError> {
        SorobanRpcClient::get_transaction_status(self, hash).await
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_deserializes_correctly() {
        let s: TransactionStatus = serde_json::from_str(r#""SUCCESS""#).unwrap();
        assert_eq!(s, TransactionStatus::Success);

        let s: TransactionStatus = serde_json::from_str(r#""PENDING""#).unwrap();
        assert_eq!(s, TransactionStatus::Pending);

        let s: TransactionStatus = serde_json::from_str(r#""FAILED""#).unwrap();
        assert_eq!(s, TransactionStatus::Failed);

        let s: TransactionStatus = serde_json::from_str(r#""NOT_FOUND""#).unwrap();
        assert_eq!(s, TransactionStatus::NotFound);

        let s: TransactionStatus = serde_json::from_str(r#""SOME_FUTURE_STATUS""#).unwrap();
        assert_eq!(s, TransactionStatus::Unknown);
    }

    #[test]
    fn simulation_result_deserializes() {
        let json = r#"{
            "cost": { "cpuInsns": "100", "memBytes": "200" },
            "footprint": "AAAA",
            "auth": [],
            "latestLedger": "1234"
        }"#;
        let result: SimulationResult = serde_json::from_str(json).unwrap();
        assert!(result.cost.is_some());
        assert_eq!(result.latest_ledger.as_deref(), Some("1234"));
    }

    #[test]
    fn send_result_deserializes() {
        let json = r#"{
            "hash": "abc123",
            "status": "PENDING",
            "latestLedger": "9999"
        }"#;
        let result: SendResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.hash, "abc123");
        assert_eq!(result.status, TransactionStatus::Pending);
    }
}
