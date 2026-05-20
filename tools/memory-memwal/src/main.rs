//! Nexus Tools for MemWal persistent memory.
//!
//! ## Pinned MemWal release
//!
//! Wire format is derived from the relayer source at tag
//! `@mysten-incubation/memwal@0.0.4` (commit `0cd0862ade`, server
//! `Cargo.toml` version `0.1.0`). See `client::MEMWAL_API_VERSION` for the
//! maintenance contract: re-audit `services/server/src/{auth,types,routes,
//! rate_limit}.rs` at any newly published tag whose Cargo version differs.
//!
//! ## Exposed tools
//!
//! Exposes seven tools under a single binary:
//!
//! - `RememberMemory`       — store a text memory and return its blob ID
//! - `RememberBulkMemories` — store up to 20 memories in one batched call
//! - `RecallMemories`       — semantic search over stored memories
//! - `AskMemory`            — memory-augmented Q&A
//! - `AnalyzeAndRemember`   — extract facts from text and store each as a memory
//! - `ForgetMemories`       — delete every memory in a namespace
//! - `StatsForAccount`      — count + stored bytes for a namespace
//!
//! ## Environment configuration
//!
//! `.env` files are loaded at startup via [`dotenvy`].
//!
//! ### Required
//!
//! | Variable | Description |
//! |---|---|
//! | `MEMWAL_DELEGATE_PRIVATE_KEY` | Hex-encoded 32-byte Ed25519 delegate private key |
//!
//! ### Recommended
//!
//! | Variable | Description |
//! |---|---|
//! | `MEMWAL_ACCOUNT_ID` | MemWal account object ID (`0x…`). Sent as `x-account-id` and embedded in the signed message — matches the JS SDK 1:1 and skips the on-chain registry scan. |
//!
//! ### Optional
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `MEMWAL_SERVER_URL` | `https://relayer.staging.memwal.ai` (testnet) | MemWal relayer base URL. Set to `https://relayer.memwal.ai` for mainnet. |

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

#[tokio::main]
async fn main() {
    // Pull env vars from a `.env` file (if one exists in cwd or any ancestor)
    // before reading any env. Existing exports take precedence.
    client::load_dotenv_if_present();

    // `bootstrap!` instantiates each Tool lazily on first request, which would
    // defer credential validation until the first /invoke. Run it eagerly so a
    // malformed MEMWAL_DELEGATE_PRIVATE_KEY fails the boot itself.
    client::validate_credentials_at_startup();

    bootstrap!([
        remember::RememberMemory,
        remember_bulk::RememberBulkMemories,
        recall::RecallMemories,
        ask::AskMemory,
        analyze::AnalyzeAndRemember,
        forget::ForgetMemories,
        stats::StatsForAccount,
    ])
}
