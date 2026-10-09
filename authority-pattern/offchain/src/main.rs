#![doc = include_str!("../README.md")]

use nexus_toolkit::bootstrap;

mod demo;

#[tokio::main]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("mock-provider") => provider_server().await,
        Some("demo") => {
            if let Err(error) = demo::run().await {
                eprintln!("local demo failed: {error:#}");
                std::process::exit(1);
            }
        }
        Some("worker") => {
            if let Err(error) = agent_api::worker::run().await {
                eprintln!("agent-api worker could not start: {error}");
                std::process::exit(1);
            }
        }
        _ => bootstrap!([
            agent_api::tools::query::QueryTool,
            agent_api::tools::retrieve_key::RetrieveKeyTool
        ]),
    }
}

async fn provider_server() {
    let bind = std::env::var("AGENT_API_MOCK_PROVIDER_BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8091".to_owned());
    let operator_key = std::env::var("AGENT_API_MOCK_OPERATOR_KEY")
        .expect("AGENT_API_MOCK_OPERATOR_KEY is required for the local mock provider");
    let database = std::env::var("AGENT_API_MOCK_PROVIDER_DB")
        .unwrap_or_else(|_| "./agent-api-mock-provider.sqlite".to_owned());
    let routes = agent_api::provider::mock_routes(database, operator_key)
        .expect("mock provider database initialization failed");
    let address: std::net::SocketAddr = bind
        .parse()
        .expect("AGENT_API_MOCK_PROVIDER_BIND_ADDR must be a socket address");
    warp::serve(routes).run(address).await;
}
