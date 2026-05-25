//! Shared Stripe response shapes used by two or more endpoints.

use {
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
};

/// Stripe Balance amount entry.
#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone)]
pub(crate) struct BalanceAmount {
    pub amount: i64,
    pub currency: String,
}

/// Stripe Charge summary as returned by `/v1/charges` list calls.
#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone)]
pub(crate) struct ChargeSummary {
    pub id: String,
    pub amount: i64,
    pub currency: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customer: Option<String>,
}
