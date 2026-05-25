//! Stripe endpoints. Each submodule is one stateless `NexusTool`.

pub(crate) const STRIPE_API_BASE: &str = "https://api.stripe.com";

pub(crate) mod confirm_payment_intent;
pub(crate) mod create_customer;
pub(crate) mod create_payment_intent;
pub(crate) mod get_balance;
pub(crate) mod get_payment_intent;
pub(crate) mod list_charges;
pub(crate) mod models;
