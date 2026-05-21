//! Nexus Tools for MemWal persistent memory. See `README.md` for the tool
//! list, env vars, and per-tool I/O contracts.
//!
//! Wire format pinned to `@mysten-incubation/memwal@0.0.4` (server Cargo
//! version `0.1.0`). [`client::MEMWAL_API_VERSION`] documents the
//! re-audit contract on tag bumps.

use nexus_toolkit::bootstrap;

mod analyze;
mod ask;
mod auth;
mod client;
mod error;
mod forget;
mod recall;
mod remember;
mod remember_bulk;
mod stats;

fn main() {
    // Install env_logger before anything else so dotenv/credential paths
    // emit through `log::*`; `bootstrap!`'s own `try_init()` becomes a no-op.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // dotenv + credential validation run single-threaded — `set_var` is
    // unsound from a multi-threaded process. `main` is the only exit site.
    client::load_dotenv_if_present();
    if let Err(reason) = client::validate_credentials_at_startup() {
        log::error!("{} {reason}", client::ENV_PRIVATE_KEY);
        std::process::exit(1);
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async {
            bootstrap!([
                remember::RememberMemory,
                remember_bulk::RememberBulkMemories,
                recall::RecallMemories,
                ask::AskMemory,
                analyze::AnalyzeAndRemember,
                forget::ForgetMemories,
                stats::StatsForAccount,
            ])
        });
}
