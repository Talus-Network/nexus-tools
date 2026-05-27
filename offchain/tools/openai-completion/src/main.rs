#![doc = include_str!("../README.md")]

use nexus_toolkit::bootstrap;

mod openai_completion;

#[tokio::main]
async fn main() {
    let _ = nexus_toolkit::env_logger::try_init();
    openai_completion::validate_config();
    bootstrap!([openai_completion::OpenaiCompletion])
}
