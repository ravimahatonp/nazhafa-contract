use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HorizonError {
    #[error("reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("http error {0}: {1}")]
    Http(u16, String),

    #[error("other: {0}")]
    Other(String),
}

impl crate::utils::retry::IsRetryable for HorizonError {
    fn is_retryable(&self) -> bool {
        match self {
            HorizonError::Reqwest(err) => {
                err.is_timeout()
                    || err.is_connect()
                    || err.status().is_some_and(|s| s.is_server_error() || s.as_u16() == 429)
            }
            HorizonError::Http(status, _) => *status >= 500 || *status == 429 || *status == 408,
            _ => false,
        }
    }
}

#[derive(Clone)]
pub struct HorizonClient {
    base_url: String,
    client: Client,
}

impl HorizonClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = Client::builder().build().expect("reqwest client");
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    /// Convenience constructor for public testnet Horizon
    pub fn public_testnet() -> Self {
        Self::new("https://horizon-testnet.stellar.org")
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, HorizonError> {
        let url = format!("{}{}", self.base_url, path);
        crate::utils::retry::retry_with_backoff(
            || async {
                let resp = self.client.get(&url).send().await?;
                let status = resp.status();
                let text = resp.text().await?;
                if !status.is_success() {
                    return Err(HorizonError::Http(status.as_u16(), text));
                }
                let parsed = serde_json::from_str(&text)?;
                Ok(parsed)
            },
            3,
            std::time::Duration::from_millis(200),
        )
        .await
    }

    pub async fn get_account(&self, address: &str) -> Result<AccountResponse, HorizonError> {
        let path = format!("/accounts/{}", address);
        self.get_json(&path).await
    }

    pub async fn get_transactions(
        &self,
        address: &str,
        cursor: Option<&str>,
    ) -> Result<TransactionPage, HorizonError> {
        let mut path = format!("/accounts/{}/transactions?limit=10&order=desc", address);
        if let Some(c) = cursor {
            path.push_str("&cursor=");
            path.push_str(c);
        }
        self.get_json(&path).await
    }

    pub async fn get_payments(
        &self,
        address: &str,
        cursor: Option<&str>,
    ) -> Result<PaymentPage, HorizonError> {
        let mut path = format!("/accounts/{}/payments?limit=10&order=desc", address);
        if let Some(c) = cursor {
            path.push_str("&cursor=");
            path.push_str(c);
        }
        self.get_json(&path).await
    }

    pub async fn get_transaction(&self, hash: &str) -> Result<TransactionDetail, HorizonError> {
        let path = format!("/transactions/{}", hash);
        self.get_json(&path).await
    }
}

// ---- Response structs (minimal, expand as needed) ----

#[derive(Debug, Deserialize)]
pub struct Balance {
    pub asset_type: String,
    #[serde(default)]
    pub asset_code: Option<String>,
    pub balance: String,
}

#[derive(Debug, Deserialize)]
pub struct AccountResponse {
    pub id: String,
    pub account_id: String,
    pub sequence: String,
    pub balances: Vec<Balance>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct TransactionSummary {
    pub id: String,
    pub paging_token: String,
    pub hash: Option<String>,
    pub created_at: Option<String>,
    pub source_account: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct TransactionPage {
    pub _embedded: EmbeddedTransactions,
}

#[derive(Debug, Deserialize)]
pub struct EmbeddedTransactions {
    pub records: Vec<TransactionSummary>,
}

#[derive(Debug, Deserialize)]
pub struct PaymentSummary {
    pub id: String,
    pub paging_token: String,
    pub source_account: Option<String>,
    pub type_: Option<String>,
    #[serde(rename = "type_i")]
    pub type_i: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct PaymentPage {
    pub _embedded: EmbeddedPayments,
}

#[derive(Debug, Deserialize)]
pub struct EmbeddedPayments {
    pub records: Vec<PaymentSummary>,
}

#[derive(Debug, Deserialize)]
pub struct TransactionDetail {
    pub id: String,
    pub hash: String,
    pub ledger: Option<u64>,
    pub created_at: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}
