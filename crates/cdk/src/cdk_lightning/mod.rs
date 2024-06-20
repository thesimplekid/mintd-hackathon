//! CDK Mint Lightning

use std::fmt;
use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use lightning_invoice::{Bolt11Invoice, ParseOrSemanticError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::InvoiceStatus;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Lightning(Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
    #[error(transparent)]
    Parse(#[from] ParseOrSemanticError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Amount(u64);

impl Amount {
    pub const ZERO: Amount = Amount(0);

    /// To Sats
    pub fn to_sat(&self) -> u64 {
        self.0 / 1000
    }

    /// From Sats
    pub fn from_sat(sat: u64) -> Self {
        Self(sat * 1000)
    }

    /// From mSats
    pub fn from_msat(msat: u64) -> Self {
        Self(msat)
    }

    /// To Sats
    pub fn to_msat(&self) -> u64 {
        self.0
    }
}

impl Default for Amount {
    fn default() -> Self {
        Amount::ZERO
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} msat", self.0)
    }
}

/// Invoice information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvoiceInfo {
    /// Payment hash of LN Invoice
    pub payment_hash: String,
    /// random hash generated by the mint to internally look up the invoice
    /// state
    pub hash: String,
    /// bolt11 invoice
    pub invoice: Bolt11Invoice,
    /// Amount
    pub amount: Amount,
    /// Status
    pub status: InvoiceStatus,
    /// Memo
    pub memo: String,
    /// Invoice paid at
    pub confirmed_at: Option<u64>,
}

impl InvoiceInfo {
    pub fn new(
        payment_hash: &str,
        hash: &str,
        invoice: Bolt11Invoice,
        amount: Amount,
        status: InvoiceStatus,
        memo: &str,
        confirmed_at: Option<u64>,
    ) -> Self {
        Self {
            payment_hash: payment_hash.to_string(),
            hash: hash.to_string(),
            invoice,
            amount,
            status,
            memo: memo.to_string(),
            confirmed_at,
        }
    }

    /// Invoice info as json
    pub fn as_json(&self) -> Result<String, Error> {
        Ok(serde_json::to_string(self)?)
    }
}

#[async_trait]
pub trait MintLightning {
    type Err: Into<Error> + From<Error>;

    /// Create a new invoice
    async fn create_invoice(
        &self,
        amount: Amount,
        description: String,
        unix_expiry: u64,
    ) -> Result<Bolt11Invoice, Self::Err>;

    /// Pay bolt11 invoice
    async fn pay_invoice(
        &self,
        bolt11: Bolt11Invoice,
        partial_msat: Option<Amount>,
        max_fee: Option<Amount>,
    ) -> Result<PayInvoiceResponse, Self::Err>;

    /// Listen for invoices to be paid
    async fn wait_invoice(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Option<Bolt11Invoice>> + Send>>, Self::Err>;

    /// Get ln node balance
    async fn get_balance(&self) -> Result<BalanceResponse, Self::Err>;
}

/// Balance response
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BalanceResponse {
    pub on_chain_spendable: Amount,
    pub on_chain_total: Amount,
    pub ln: Amount,
}

/// Pay invoice response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayInvoiceResponse {
    pub payment_hash: String,
    pub payment_preimage: Option<String>,
    pub status: InvoiceStatus,
    pub total_spent: Amount,
}
