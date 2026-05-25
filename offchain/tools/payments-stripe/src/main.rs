#![doc = include_str!("../README.md")]
#![allow(clippy::large_enum_variant)]

use nexus_toolkit::bootstrap;

mod error;
mod stripe_client;
mod tools;

fn main() {
    // Install env_logger before anything else so dotenv/credential paths
    // emit through `log::*`; `bootstrap!`'s own `try_init()` becomes a no-op.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // `--meta` is an introspection mode that emits the tool's FQN/URL/
    // schema list without standing up the runtime. CI's prepare step
    // invokes it from a fresh image with no env, so it must not require
    // runtime credentials. `nexus_toolkit::bootstrap!` handles --meta
    // and exits before any HTTP call is made.
    let meta_only = std::env::args().any(|a| a == "--meta");

    if !meta_only {
        // dotenv + credential validation run single-threaded — `set_var`
        // is unsound from a multi-threaded process. `main` is the only
        // exit site.
        stripe_client::load_dotenv_if_present();
        if let Err(reason) = stripe_client::validate_credentials_at_startup() {
            log::error!("{} {reason}", stripe_client::ENV_API_KEY);
            std::process::exit(1);
        }
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async {
            bootstrap!([
                tools::create_payment_intent::CreatePaymentIntent,
                tools::get_payment_intent::GetPaymentIntent,
                tools::confirm_payment_intent::ConfirmPaymentIntent,
                tools::create_customer::CreateCustomer,
                tools::get_balance::GetBalance,
                tools::list_charges::ListCharges,
            ])
        });
}
