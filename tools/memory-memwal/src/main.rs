//! Nexus Tools for MemWal persistent memory.
//!
//! Exposes four tools under a single binary:
//!
//! - `RememberMemory`  — store a text memory and return its blob ID
//! - `RecallMemories`  — semantic search over stored memories
//! - `AskMemory`       — memory-augmented Q&A
//! - `AnalyzeAndRemember` — extract facts from text and store each as a memory
//!
//! ## Required environment variables
//!
//! | Variable | Description |
//! |---|---|
//! | `MEMWAL_DELEGATE_PRIVATE_KEY` | Hex-encoded 32-byte Ed25519 delegate private key |
//!
//! ## Optional environment variables
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `MEMWAL_SERVER_URL` | `https://relayer.memwal.ai` | MemWal relayer base URL |

use nexus_toolkit::bootstrap;

mod analyze;
mod ask;
mod auth;
mod client;
mod error;
mod recall;
mod remember;

#[cfg(test)]
mod integration;

#[tokio::main]
async fn main() {
    bootstrap!([
        remember::RememberMemory,
        recall::RecallMemories,
        ask::AskMemory,
        analyze::AnalyzeAndRemember,
    ])
}
