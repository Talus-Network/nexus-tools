#![doc = include_str!("../README.md")]
#![allow(clippy::large_enum_variant)]

use nexus_toolkit::bootstrap;

mod error;
mod stripe_client;
mod tools;

#[tokio::main]
async fn main() {
    bootstrap!([
        tools::create_payment_intent::CreatePaymentIntent,
        tools::get_payment_intent::GetPaymentIntent,
        tools::confirm_payment_intent::ConfirmPaymentIntent,
        tools::create_customer::CreateCustomer,
        tools::get_balance::GetBalance,
        tools::list_charges::ListCharges,
    ]);
}
