//! In-memory MemWal-compatible HTTP server for integration tests.
//!
//! Implements the MemWal relayer API with zero external dependencies:
//! no Sui, no SEAL, no Walrus, no PostgreSQL. All storage is in-memory.
//!
//! **Auth:** Ed25519 signatures are validated using the same message format as
//! the production relayer (`{ts}.{METHOD}.{path}.{sha256hex(body)}`), but any
//! valid key pair is accepted — there is no on-chain account registry.
//!
//! **Embeddings:** Deterministic SHA-256-based: the text hash is cycled to fill
//! a 1536-dim float vector which is then L2-normalized. The same text always
//! produces the same vector, so a recall query with the exact stored text yields
//! cosine distance ≈ 0.
//!
//! **Jobs:** All remember/analyze jobs complete synchronously. The first poll to
//! `GET /api/remember/:job_id` always returns `"completed"`.
//!
//! ## Usage in tests
//!
//! ```rust,no_run
//! #[tokio::test]
//! async fn my_test() {
//!     let port = memwal_test_server::start().await;
//!     // server is now listening on 127.0.0.1:{port}
//! }
//! ```

/// MemWal relayer version this server is compatible with.
///
/// Source of truth: `GET https://relayer.memwal.ai/health` → `{"version":"..."}`.
/// There is no published OpenAPI spec; the API was implemented from the prose
/// documentation at <https://docs.memwal.ai>.
///
/// Must equal `crate::client::MEMWAL_API_VERSION` in the `memory-memwal` crate.
/// The `api_version_constants_match` integration test asserts equality so a
/// mismatch surfaces as a test failure rather than a silent divergence.
pub const MEMWAL_API_VERSION: &str = "0.1.0";

use {
    bytes::Bytes,
    ed25519_dalek::{Signature, VerifyingKey},
    sha2::{Digest, Sha256},
    std::{
        collections::HashMap,
        convert::Infallible,
        sync::{Arc, Mutex},
    },
    warp::{Filter, Reply},
};

// ---------------------------------------------------------------------------
// In-memory state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StoredMemory {
    text: String,
    blob_id: String,
    embedding: Vec<f32>,
    namespace: String,
}

struct State {
    memories: Vec<StoredMemory>,
    /// job_id → blob_id; all jobs are immediately "completed".
    jobs: HashMap<String, String>,
    next_id: u64,
}

impl State {
    fn new() -> Self {
        Self {
            memories: Vec::new(),
            jobs: HashMap::new(),
            next_id: 0,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Store a single memory text and return `(job_id, blob_id)`.
    fn store(&mut self, text: String, namespace: String) -> (String, String) {
        let n = self.alloc_id();
        let blob_id = format!("blob-{n}");
        let job_id = format!("job-{n}");
        self.memories.push(StoredMemory {
            embedding: embed(&text),
            text,
            blob_id: blob_id.clone(),
            namespace,
        });
        self.jobs.insert(job_id.clone(), blob_id.clone());
        (job_id, blob_id)
    }
}

type SharedState = Arc<Mutex<State>>;

// ---------------------------------------------------------------------------
// Deterministic embedding
// ---------------------------------------------------------------------------

/// SHA-256 of text cycled to 1536 floats, L2-normalized.
///
/// Identical texts produce identical vectors (cosine distance 0).
/// Different texts produce different vectors (cosine distance > 0).
fn embed(text: &str) -> Vec<f32> {
    let hash = Sha256::digest(text.as_bytes());
    let raw: Vec<f32> = (0..1536_usize)
        .map(|i| (hash[i % 32] as f32 / 127.5) - 1.0)
        .collect();
    let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        raw.into_iter().map(|x| x / norm).collect()
    } else {
        raw
    }
}

/// Cosine distance for two L2-normalized vectors: 1 − dot_product.
fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    (1.0 - dot.clamp(-1.0, 1.0)) as f64
}

/// Split text on ". " boundaries into non-empty sentences.
///
/// "A is B. C is D." → ["A is B", "C is D"]
fn split_sentences(text: &str) -> Vec<String> {
    text.split(". ")
        .map(|s| s.trim().trim_end_matches('.').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Ed25519 auth validation
// ---------------------------------------------------------------------------

/// Verify the MemWal Ed25519 request signature.
///
/// Message: `{timestamp}.{METHOD}.{path}.{sha256hex(body)}`
fn verify_auth(
    pubkey_hex: &str,
    sig_hex: &str,
    ts: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<(), String> {
    let pubkey_bytes: [u8; 32] = hex::decode(pubkey_hex)
        .map_err(|e| format!("bad x-public-key: {e}"))?
        .try_into()
        .map_err(|_| "x-public-key must be 32 bytes".to_string())?;

    let sig_bytes: [u8; 64] = hex::decode(sig_hex)
        .map_err(|e| format!("bad x-signature: {e}"))?
        .try_into()
        .map_err(|_| "x-signature must be 64 bytes".to_string())?;

    let key =
        VerifyingKey::from_bytes(&pubkey_bytes).map_err(|e| format!("invalid public key: {e}"))?;

    let sig = Signature::from_bytes(&sig_bytes);
    let body_hash = hex::encode(Sha256::digest(body));
    let msg = format!("{ts}.{method}.{path}.{body_hash}");

    key.verify_strict(msg.as_bytes(), &sig)
        .map_err(|e| format!("signature verification failed: {e}"))
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

type JsonReply = warp::reply::WithStatus<warp::reply::Json>;

fn ok(v: serde_json::Value) -> JsonReply {
    warp::reply::with_status(warp::reply::json(&v), warp::http::StatusCode::OK)
}

fn accepted(v: serde_json::Value) -> JsonReply {
    warp::reply::with_status(warp::reply::json(&v), warp::http::StatusCode::ACCEPTED)
}

fn bad_request(msg: &str) -> JsonReply {
    warp::reply::with_status(
        warp::reply::json(&serde_json::json!({"error": msg})),
        warp::http::StatusCode::BAD_REQUEST,
    )
}

fn unauthorized(msg: &str) -> JsonReply {
    warp::reply::with_status(
        warp::reply::json(&serde_json::json!({"error": msg})),
        warp::http::StatusCode::UNAUTHORIZED,
    )
}

fn not_found(msg: &str) -> JsonReply {
    warp::reply::with_status(
        warp::reply::json(&serde_json::json!({"error": msg})),
        warp::http::StatusCode::NOT_FOUND,
    )
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

async fn handle_health() -> Result<JsonReply, warp::Rejection> {
    Ok(ok(serde_json::json!({
        "status": "ok",
        "version": MEMWAL_API_VERSION,
    })))
}

/// `POST /api/remember` — store one memory and return a job_id (202 Accepted).
async fn handle_remember(
    pubkey: String,
    sig: String,
    ts: String,
    body: Bytes,
    state: SharedState,
) -> Result<JsonReply, warp::Rejection> {
    if let Err(e) = verify_auth(&pubkey, &sig, &ts, "POST", "/api/remember", &body) {
        return Ok(unauthorized(&e));
    }

    #[derive(serde::Deserialize)]
    struct Req {
        text: String,
        namespace: Option<String>,
    }

    let req: Req = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return Ok(bad_request(&format!("invalid JSON: {e}"))),
    };

    let ns = req.namespace.unwrap_or_else(|| "default".to_string());
    let (job_id, _) = state.lock().unwrap().store(req.text, ns);
    Ok(accepted(serde_json::json!({"job_id": job_id})))
}

/// `GET /api/remember/:job_id` — always returns "completed" for known jobs.
async fn handle_job_status(
    job_id: String,
    pubkey: String,
    sig: String,
    ts: String,
    state: SharedState,
) -> Result<JsonReply, warp::Rejection> {
    let path = format!("/api/remember/{job_id}");
    if let Err(e) = verify_auth(&pubkey, &sig, &ts, "GET", &path, b"") {
        return Ok(unauthorized(&e));
    }

    let s = state.lock().unwrap();
    match s.jobs.get(&job_id) {
        Some(blob_id) => Ok(ok(serde_json::json!({
            "job_id": job_id,
            "status": "done",
            "blob_id": blob_id,
            "owner": "test",
            "namespace": "default",
        }))),
        None => Ok(not_found(&format!("job {job_id} not found"))),
    }
}

/// `POST /api/recall` — cosine-similarity search over stored memories.
async fn handle_recall(
    pubkey: String,
    sig: String,
    ts: String,
    body: Bytes,
    state: SharedState,
) -> Result<JsonReply, warp::Rejection> {
    if let Err(e) = verify_auth(&pubkey, &sig, &ts, "POST", "/api/recall", &body) {
        return Ok(unauthorized(&e));
    }

    #[derive(serde::Deserialize)]
    struct Req {
        query: String,
        limit: Option<usize>,
        namespace: Option<String>,
    }

    let req: Req = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return Ok(bad_request(&format!("invalid JSON: {e}"))),
    };

    let limit = req.limit.unwrap_or(10);
    let qvec = embed(&req.query);

    let s = state.lock().unwrap();
    let mut hits: Vec<_> = s
        .memories
        .iter()
        .filter(|m| req.namespace.as_deref().map_or(true, |ns| m.namespace == ns))
        .map(|m| (m, cosine_distance(&qvec, &m.embedding)))
        .collect();

    hits.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit);

    let total = hits.len();
    let results: Vec<serde_json::Value> = hits
        .into_iter()
        .map(|(m, dist)| {
            // RecallResult on the server has no namespace field.
            serde_json::json!({
                "blob_id":  m.blob_id,
                "text":     m.text,
                "distance": dist,
            })
        })
        .collect();

    Ok(ok(serde_json::json!({"results": results, "total": total})))
}

/// `POST /api/ask` — retrieve top matches and build a template answer.
async fn handle_ask(
    pubkey: String,
    sig: String,
    ts: String,
    body: Bytes,
    state: SharedState,
) -> Result<JsonReply, warp::Rejection> {
    if let Err(e) = verify_auth(&pubkey, &sig, &ts, "POST", "/api/ask", &body) {
        return Ok(unauthorized(&e));
    }

    #[derive(serde::Deserialize)]
    struct Req {
        question: String,
        namespace: Option<String>,
        limit: Option<usize>,
    }

    let req: Req = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return Ok(bad_request(&format!("invalid JSON: {e}"))),
    };

    let limit = req.limit.unwrap_or(3);
    let qvec = embed(&req.question);

    let s = state.lock().unwrap();
    let mut hits: Vec<_> = s
        .memories
        .iter()
        .filter(|m| req.namespace.as_deref().map_or(true, |ns| m.namespace == ns))
        .map(|m| (m, cosine_distance(&qvec, &m.embedding)))
        .collect();

    hits.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit);

    let answer = if hits.is_empty() {
        "No relevant memories found.".to_string()
    } else {
        let facts: Vec<&str> = hits.iter().map(|(m, _)| m.text.as_str()).collect();
        format!("Based on stored memories: {}", facts.join("; "))
    };

    let memories: Vec<serde_json::Value> = hits
        .iter()
        .map(|(m, dist)| {
            serde_json::json!({
                "blob_id":  m.blob_id,
                "text":     m.text,
                "distance": dist,
            })
        })
        .collect();

    Ok(ok(serde_json::json!({
        "answer":       answer,
        "memories_used": memories.len(),
        "memories":     memories,
    })))
}

/// `POST /api/analyze` — split text into sentences, store each, return job_ids.
async fn handle_analyze(
    pubkey: String,
    sig: String,
    ts: String,
    body: Bytes,
    state: SharedState,
) -> Result<JsonReply, warp::Rejection> {
    if let Err(e) = verify_auth(&pubkey, &sig, &ts, "POST", "/api/analyze", &body) {
        return Ok(unauthorized(&e));
    }

    #[derive(serde::Deserialize)]
    struct Req {
        text: String,
        namespace: Option<String>,
    }

    let req: Req = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return Ok(bad_request(&format!("invalid JSON: {e}"))),
    };

    let ns = req.namespace.unwrap_or_else(|| "default".to_string());
    let sentences = split_sentences(&req.text);

    let mut s = state.lock().unwrap();
    let job_ids: Vec<String> = sentences
        .into_iter()
        .map(|sentence| {
            let (job_id, _) = s.store(sentence, ns.clone());
            job_id
        })
        .collect();

    Ok(ok(serde_json::json!({"job_ids": job_ids})))
}

// ---------------------------------------------------------------------------
// Rejection handler
// ---------------------------------------------------------------------------

async fn handle_rejection(err: warp::Rejection) -> Result<JsonReply, Infallible> {
    let code = if err.is_not_found() {
        warp::http::StatusCode::NOT_FOUND
    } else {
        warp::http::StatusCode::BAD_REQUEST
    };
    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({"error": format!("{err:?}")})),
        code,
    ))
}

// ---------------------------------------------------------------------------
// Route wiring
// ---------------------------------------------------------------------------

fn with_state(
    state: SharedState,
) -> impl Filter<Extract = (SharedState,), Error = Infallible> + Clone {
    warp::any().map(move || state.clone())
}

fn build_routes(
    state: SharedState,
) -> impl Filter<Extract = impl Reply, Error = Infallible> + Clone {
    let health = warp::get()
        .and(warp::path("health"))
        .and(warp::path::end())
        .and_then(handle_health);

    let remember_post = warp::post()
        .and(warp::path!("api" / "remember"))
        .and(warp::header("x-public-key"))
        .and(warp::header("x-signature"))
        .and(warp::header("x-timestamp"))
        .and(warp::body::bytes())
        .and(with_state(state.clone()))
        .and_then(handle_remember);

    let remember_get = warp::get()
        .and(warp::path!("api" / "remember" / String))
        .and(warp::header("x-public-key"))
        .and(warp::header("x-signature"))
        .and(warp::header("x-timestamp"))
        .and(with_state(state.clone()))
        .and_then(handle_job_status);

    let recall = warp::post()
        .and(warp::path!("api" / "recall"))
        .and(warp::header("x-public-key"))
        .and(warp::header("x-signature"))
        .and(warp::header("x-timestamp"))
        .and(warp::body::bytes())
        .and(with_state(state.clone()))
        .and_then(handle_recall);

    let ask = warp::post()
        .and(warp::path!("api" / "ask"))
        .and(warp::header("x-public-key"))
        .and(warp::header("x-signature"))
        .and(warp::header("x-timestamp"))
        .and(warp::body::bytes())
        .and(with_state(state.clone()))
        .and_then(handle_ask);

    let analyze = warp::post()
        .and(warp::path!("api" / "analyze"))
        .and(warp::header("x-public-key"))
        .and(warp::header("x-signature"))
        .and(warp::header("x-timestamp"))
        .and(warp::body::bytes())
        .and(with_state(state))
        .and_then(handle_analyze);

    health
        .or(remember_post)
        .or(remember_get)
        .or(recall)
        .or(ask)
        .or(analyze)
        .recover(handle_rejection)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start the test server on a random free port. Returns the bound port.
///
/// The server runs in a background tokio task for the lifetime of the process.
/// Intended to be called once from `tokio::sync::OnceCell::get_or_init` so a
/// single server instance is shared across all tests in a binary.
pub async fn start() -> u16 {
    let port = portpicker::pick_unused_port().expect("no free port available");
    let state = Arc::new(Mutex::new(State::new()));
    let routes = build_routes(state);
    tokio::spawn(warp::serve(routes).run(([127, 0, 0, 1], port)));
    // Brief pause for the OS to bind the socket before tests issue requests.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    port
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `embed` is deterministic: the same text always yields the same vector.
    /// Failure mode caught: non-deterministic embedding breaks recall for exact matches.
    #[test]
    fn embed_is_deterministic() {
        let a = embed("hello world");
        let b = embed("hello world");
        assert_eq!(a, b, "embed must be deterministic for identical input");
    }

    /// `embed` produces a unit-norm (L2 ≈ 1.0) vector.
    /// Failure mode caught: un-normalized vectors produce wrong cosine distances.
    #[test]
    fn embed_produces_unit_norm_vector() {
        let v = embed("test text for normalization");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "embedding must be L2-normalized; got norm={norm}"
        );
    }

    /// `cosine_distance` of a vector with itself is ≈ 0.
    /// Failure mode caught: exact recall matches score non-zero distance and get filtered out.
    #[test]
    fn cosine_distance_self_is_zero() {
        let v = embed("the quick brown fox");
        let d = cosine_distance(&v, &v);
        assert!(
            d < 1e-6,
            "cosine distance of a vector with itself must be ≈ 0; got {d}"
        );
    }

    /// `cosine_distance` of two different texts is > 0.
    /// Failure mode caught: all texts map to the same vector, making recall meaningless.
    #[test]
    fn cosine_distance_different_texts_nonzero() {
        let a = embed("the quick brown fox");
        let b = embed("Paris is the capital of France");
        let d = cosine_distance(&a, &b);
        assert!(
            d > 0.0,
            "cosine distance between distinct texts must be > 0; got {d}"
        );
    }

    /// `split_sentences` splits on ". " and strips trailing dots.
    /// Failure mode caught: multi-fact documents produce only one job in analyze.
    #[test]
    fn split_sentences_extracts_four_facts() {
        let text = "Alice is an engineer. Bob is a manager. \
                    Carol leads design. Dave handles ops.";
        let facts = split_sentences(text);
        assert_eq!(
            facts.len(),
            4,
            "four-sentence input must produce four facts; got: {facts:?}"
        );
        assert_eq!(facts[0], "Alice is an engineer");
        assert_eq!(facts[3], "Dave handles ops");
    }

    /// `verify_auth` accepts a correctly signed request.
    /// Failure mode caught: the test server rejects all requests, making integration tests useless.
    #[test]
    fn verify_auth_accepts_valid_signature() {
        use ed25519_dalek::{Signer, SigningKey};

        let key_bytes = [0x42u8; 32];
        let signing_key = SigningKey::from_bytes(&key_bytes);
        let pubkey_hex = hex::encode(signing_key.verifying_key().to_bytes());

        let body = b"{\"text\":\"hello\"}";
        let body_hash = hex::encode(Sha256::digest(body));
        let ts = "1700000000";
        let msg = format!("{ts}.POST./api/remember.{body_hash}");
        let sig = signing_key.sign(msg.as_bytes());
        let sig_hex = hex::encode(sig.to_bytes());

        verify_auth(&pubkey_hex, &sig_hex, ts, "POST", "/api/remember", body)
            .expect("valid signature must be accepted");
    }

    /// `verify_auth` rejects a tampered body (wrong body hash in message).
    /// Failure mode caught: server accepts requests whose body was modified after signing.
    #[test]
    fn verify_auth_rejects_tampered_body() {
        use ed25519_dalek::{Signer, SigningKey};

        let key_bytes = [0x42u8; 32];
        let signing_key = SigningKey::from_bytes(&key_bytes);
        let pubkey_hex = hex::encode(signing_key.verifying_key().to_bytes());

        // Sign over the original body.
        let original_body = b"{\"text\":\"hello\"}";
        let body_hash = hex::encode(Sha256::digest(original_body));
        let ts = "1700000000";
        let msg = format!("{ts}.POST./api/remember.{body_hash}");
        let sig = signing_key.sign(msg.as_bytes());
        let sig_hex = hex::encode(sig.to_bytes());

        // Present a different body with the same signature → must fail.
        let tampered_body = b"{\"text\":\"TAMPERED\"}";
        let result = verify_auth(&pubkey_hex, &sig_hex, ts, "POST", "/api/remember", tampered_body);
        assert!(
            result.is_err(),
            "tampered body must cause signature verification to fail"
        );
    }
}
