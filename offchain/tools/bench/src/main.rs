#![doc = include_str!("../README.md")]

use nexus_toolkit::bootstrap;

mod load;

#[tokio::main]
async fn main() {
    bootstrap!([load::BenchLoad])
}
