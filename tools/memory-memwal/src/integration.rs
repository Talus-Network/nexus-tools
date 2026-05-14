//! Integration tests for the memory-memwal tools.
//!
//! Tests run against an in-memory MemWal-compatible server by default (zero
//! Sui/SEAL/Walrus/PostgreSQL dependencies). When both `MEMWAL_SERVER_URL` and
//! `MEMWAL_DELEGATE_PRIVATE_KEY` are set in the environment the same tests run
//! against that server instead (e.g. a staging relayer).
//!
//! ## Running locally (default — no credentials needed)
//!
//! ```sh
//! cargo nextest run --package memory-memwal integration
//! ```
//!
//! ## Running against staging
//!
//! ```sh
//! MEMWAL_DELEGATE_PRIVATE_KEY=<hex_key> \
//! MEMWAL_SERVER_URL=https://relayer.staging.memwal.ai \
//!   cargo nextest run --package memory-memwal integration
//! ```
//!
//! Tests run serially to stay within the per-key rate limit on staging.

use {
    crate::{
        analyze::{AnalyzeAndRemember, Output as AnalyzeOutput},
        ask::{AskMemory, Output as AskOutput},
        recall::{Output as RecallOutput, RecallMemories},
        remember::{Output as RememberOutput, RememberMemory},
    },
    nexus_toolkit::NexusTool,
    serde_json::json,
    serial_test::serial,
    std::time::{SystemTime, UNIX_EPOCH},
    tokio::sync::OnceCell,
};

// ---------------------------------------------------------------------------
// Test environment bootstrap
// ---------------------------------------------------------------------------

static LOCAL_SERVER: OnceCell<String> = OnceCell::const_new();

/// Ensure `MEMWAL_SERVER_URL` and `MEMWAL_DELEGATE_PRIVATE_KEY` are set.
///
/// When both env vars are already present the function is a no-op (staging
/// mode). Otherwise it starts the in-process test server on a random port and
/// sets both env vars so that `Tool::new()` picks them up automatically.
///
/// # Safety
/// `std::env::set_var` is called here. It is safe because all integration
/// tests are annotated `#[serial]`, which guarantees they never execute
/// concurrently within the same process.
async fn ensure_test_env() {
    let has_url = std::env::var("MEMWAL_SERVER_URL").is_ok();
    let has_key = std::env::var("MEMWAL_DELEGATE_PRIVATE_KEY").is_ok();
    if has_url && has_key {
        return;
    }
    LOCAL_SERVER
        .get_or_init(|| async {
            let port = memwal_test_server::start().await;
            let url = format!("http://127.0.0.1:{port}");
            std::env::set_var("MEMWAL_SERVER_URL", &url);
            std::env::set_var(
                "MEMWAL_DELEGATE_PRIVATE_KEY",
                hex::encode([0x42u8; 32]),
            );
            url
        })
        .await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a namespace that is unique per test invocation so that memories
/// written by one test run do not pollute another.
fn unique_ns(label: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    format!("nexus-ci-{label}-{ts}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Both crates declare `MEMWAL_API_VERSION`; this test asserts they are equal.
/// Failure mode caught: one crate is updated to a new API revision while the other is not,
/// causing the test server to silently misrepresent the production API contract.
#[tokio::test]
#[serial]
async fn api_version_constants_match() {
    assert_eq!(
        crate::client::MEMWAL_API_VERSION,
        memwal_test_server::MEMWAL_API_VERSION,
        "tools and test server must target the same MemWal API version"
    );
}

/// Calls the tool's `health()` method and verifies it returns HTTP 200.
/// Failure mode caught: health endpoint unreachable or delegate key missing/malformed.
#[tokio::test]
#[serial]
async fn health_check() {
    ensure_test_env().await;
    let tool = RememberMemory::new().await;
    let status = tool.health().await.expect("health() must not return Err");
    assert_eq!(
        status.as_u16(),
        200,
        "health check must return 200 OK"
    );
}

/// Stores a unique text memory and asserts the server returns a non-empty blob_id.
/// Failure mode caught: the remember pipeline fails silently or returns an empty blob_id.
#[tokio::test]
#[serial]
async fn remember_returns_blob_id() {
    ensure_test_env().await;
    let tool = RememberMemory::new().await;
    let ns = unique_ns("remember");
    let input = serde_json::from_value(json!({
        "text": format!("integration test memory in namespace {ns}"),
        "namespace": ns,
    }))
    .expect("valid input");

    match tool.invoke(input).await {
        RememberOutput::Ok { blob_id } => {
            assert!(
                !blob_id.is_empty(),
                "blob_id must be a non-empty reference"
            );
        }
        RememberOutput::Err { reason } => panic!("remember failed: {reason}"),
    }
}

/// Stores a uniquely-worded memory, recalls by the same text, and asserts it
/// appears in the results.
/// Failure mode caught: the write→embed→index→search pipeline is broken, or
/// recall returns results from a different namespace.
#[tokio::test]
#[serial]
async fn recall_returns_stored_memory() {
    ensure_test_env().await;
    let ns = unique_ns("recall");
    let unique_text = format!("the purple elephant dances at midnight in namespace {ns}");

    let remember = RememberMemory::new().await;
    let store_input = serde_json::from_value(json!({
        "text": unique_text,
        "namespace": ns,
    }))
    .expect("valid input");

    match remember.invoke(store_input).await {
        RememberOutput::Ok { .. } => {}
        RememberOutput::Err { reason } => panic!("remember failed: {reason}"),
    }

    let recall = RecallMemories::new().await;
    let recall_input = serde_json::from_value(json!({
        "query": unique_text,
        "limit": 5,
        "namespace": ns,
    }))
    .expect("valid input");

    match recall.invoke(recall_input).await {
        RecallOutput::Ok { results } => {
            assert!(
                !results.is_empty(),
                "recall must return at least the memory we just stored"
            );
            let found = results.iter().any(|r| r.text == unique_text);
            assert!(
                found,
                "stored text must appear in recall results; got: {:?}",
                results.iter().map(|r| &r.text).collect::<Vec<_>>()
            );
        }
        RecallOutput::Err { reason } => panic!("recall failed: {reason}"),
    }
}

/// Stores a fact, asks a question about it, and asserts a non-empty answer with
/// at least one source is returned.
/// Failure mode caught: the ask pipeline fails to retrieve the stored memory or
/// the LLM/template call returns an empty answer.
#[tokio::test]
#[serial]
async fn ask_returns_answer_with_sources() {
    ensure_test_env().await;
    let ns = unique_ns("ask");

    let remember = RememberMemory::new().await;
    let store_input = serde_json::from_value(json!({
        "text": "The Nexus protocol uses the Talus network for verifiable AI agent execution.",
        "namespace": ns,
    }))
    .expect("valid input");

    match remember.invoke(store_input).await {
        RememberOutput::Ok { .. } => {}
        RememberOutput::Err { reason } => panic!("remember failed: {reason}"),
    }

    let ask = AskMemory::new().await;
    let ask_input = serde_json::from_value(json!({
        "question": "What protocol does Talus use for AI agent execution?",
        "namespace": ns,
        "limit": 3,
    }))
    .expect("valid input");

    match ask.invoke(ask_input).await {
        AskOutput::Ok { answer, sources } => {
            assert!(!answer.is_empty(), "answer must not be empty");
            assert!(
                !sources.is_empty(),
                "at least one source memory must be cited"
            );
        }
        AskOutput::Err { reason } => panic!("ask failed: {reason}"),
    }
}

/// Submits a multi-sentence document for fact extraction and asserts at least
/// one memory-write job is enqueued.
/// Failure mode caught: analyze endpoint returns zero jobs for a document that
/// clearly contains multiple extractable facts.
#[tokio::test]
#[serial]
async fn analyze_enqueues_jobs_for_factual_text() {
    ensure_test_env().await;
    let ns = unique_ns("analyze");
    let analyze = AnalyzeAndRemember::new().await;
    let input = serde_json::from_value(json!({
        "text": "Alice is a software engineer. Bob is a product manager. \
                 Carol leads the design team. Dave handles operations.",
        "namespace": ns,
    }))
    .expect("valid input");

    match analyze.invoke(input).await {
        AnalyzeOutput::Ok { job_count } => {
            assert!(
                job_count > 0,
                "a document with four facts must enqueue at least one job; got 0"
            );
        }
        AnalyzeOutput::Err { reason } => panic!("analyze failed: {reason}"),
    }
}

/// Stores a memory in namespace A, recalls from namespace B, and asserts the
/// results are empty — namespaces must not bleed into each other.
/// Failure mode caught: server ignores the namespace parameter and returns
/// memories across all namespaces.
#[tokio::test]
#[serial]
async fn namespace_isolation() {
    ensure_test_env().await;
    let ns_a = unique_ns("isolation-a");
    let ns_b = unique_ns("isolation-b");

    let remember = RememberMemory::new().await;
    let store_input = serde_json::from_value(json!({
        "text": "secret data that must stay in namespace A",
        "namespace": ns_a,
    }))
    .expect("valid input");

    match remember.invoke(store_input).await {
        RememberOutput::Ok { .. } => {}
        RememberOutput::Err { reason } => panic!("remember failed: {reason}"),
    }

    let recall = RecallMemories::new().await;
    let recall_input = serde_json::from_value(json!({
        "query": "secret data that must stay in namespace A",
        "limit": 5,
        "namespace": ns_b,
    }))
    .expect("valid input");

    match recall.invoke(recall_input).await {
        RecallOutput::Ok { results } => {
            assert!(
                results.is_empty(),
                "namespace B must not see memories stored in namespace A; \
                 got {} result(s)",
                results.len()
            );
        }
        RecallOutput::Err { reason } => panic!("recall from namespace B failed: {reason}"),
    }
}
