# Memory Tools (MemWal)

A Nexus Tool bundle exposing four memory operations backed by the
[MemWal](https://memwal.ai) relayer: store a memory, search memories, answer
questions from memory, and extract & store facts from text.

## Build & Run

```sh
# Build (release)
cargo build --package memory-memwal --release

# Run locally — binds to 127.0.0.1:8080 by default
cargo run --package memory-memwal

# Override the bind address
BIND_ADDR=0.0.0.0:9000 cargo run --package memory-memwal
```

## Environment Variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `MEMWAL_DELEGATE_PRIVATE_KEY` | **yes** | — | Hex-encoded 32-byte Ed25519 (Elliptic Curve Digital Signature Algorithm) delegate private key |
| `MEMWAL_SERVER_URL` | no | `https://relayer.memwal.ai` | MemWal relayer base URL |

The per-invocation `server_url` input field on every tool overrides
`MEMWAL_SERVER_URL` for that single call.

## Server Endpoints

```sh
# List registered tool paths
GET /tools

# Liveness check (validates key + reaches the relayer)
GET /health

# Invoke a tool (POST, JSON body)
POST /<tool-path>/invoke
```

## Tools

---

# `xyz.taluslabs.memory.memwal.remember@1`

Store a single piece of text as a persistent memory. The call blocks until the
memory is durably written to Walrus and returns the resulting blob ID — the
next vertex in a Nexus DAG (Directed Acyclic Graph) will not activate until
the write is confirmed.

## Input

**`text`: `String`** *(required)*

The text to store as a memory.

**`namespace`: `String`** *(optional)*

Namespace used to scope this memory. Defaults to `"default"` on the server
when omitted.

**`server_url`: `String`** *(optional)*

Override the relayer URL for this invocation. Falls back to `MEMWAL_SERVER_URL`
then `https://relayer.memwal.ai`.

## Output Variants & Ports

**`ok`**

The memory was durably stored.

- **`ok.blob_id`: `String`** — Walrus blob identifier for the stored memory.

**`err`**

The store operation failed.

- **`err.reason`: `String`** — Human-readable error description.

## Example

```sh
curl -s -X POST http://127.0.0.1:8080/memory/remember/invoke \
  -H 'Content-Type: application/json' \
  -d '{"text": "Paris is the capital of France"}'
# {"ok":{"blob_id":"<walrus-blob-id>"}}

# With an explicit namespace
curl -s -X POST http://127.0.0.1:8080/memory/remember/invoke \
  -H 'Content-Type: application/json' \
  -d '{"text": "Alice works at ACME", "namespace": "people"}'
# {"ok":{"blob_id":"<walrus-blob-id>"}}
```

---

# `xyz.taluslabs.memory.memwal.recall@1`

Semantic search over stored memories. Returns the closest matches ranked by
cosine distance — lower distance means more relevant.

## Input

**`query`: `String`** *(required)*

Natural-language query to search for relevant memories.

**`limit`: `u32`** *(optional)*

Maximum number of results to return. Server default applies when omitted.

**`namespace`: `String`** *(optional)*

Namespace to search within. Searches the `"default"` namespace when omitted.

**`server_url`: `String`** *(optional)*

Override the relayer URL for this invocation.

## Output Variants & Ports

**`ok`**

Search completed. The result list may be empty if nothing matched.

- **`ok.results`: `Array`** — Memories ranked by relevance, each containing:
  - **`text`: `String`** — The stored text of the memory.
  - **`blob_id`: `String`** — Walrus blob identifier.
  - **`distance`: `f64`** — Cosine distance from the query vector (lower = more relevant).
  - **`namespace`: `String`** — Namespace the memory belongs to.

**`err`**

The search failed.

- **`err.reason`: `String`** — Human-readable error description.

## Example

```sh
curl -s -X POST http://127.0.0.1:8080/memory/recall/invoke \
  -H 'Content-Type: application/json' \
  -d '{"query": "capital cities in Europe", "limit": 3}'
# {"ok":{"results":[{"text":"Paris is the capital of France","blob_id":"...","distance":0.12,"namespace":"default"}]}}

# Empty result when nothing matches
curl -s -X POST http://127.0.0.1:8080/memory/recall/invoke \
  -H 'Content-Type: application/json' \
  -d '{"query": "something completely unknown"}'
# {"ok":{"results":[]}}
```

---

# `xyz.taluslabs.memory.memwal.ask@1`

Memory-augmented question answering. The relayer retrieves the most relevant
stored memories for the question, injects them as context into an LLM (Large
Language Model) prompt, and returns the generated answer together with the
source memories that informed it.

## Input

**`question`: `String`** *(required)*

The question to answer using stored memories as context.

**`namespace`: `String`** *(optional)*

Namespace to retrieve memories from. Uses the `"default"` namespace when
omitted.

**`limit`: `u32`** *(optional)*

Maximum number of source memories to inject as context. Server default applies
when omitted.

**`server_url`: `String`** *(optional)*

Override the relayer URL for this invocation.

## Output Variants & Ports

**`ok`**

Question answered successfully.

- **`ok.answer`: `String`** — LLM-generated answer.
- **`ok.sources`: `Array`** — Memories used as context, each containing:
  - **`blob_id`: `String`** — Walrus blob identifier.
  - **`text`: `String`** — The stored text of the memory.
  - **`distance`: `f64`** — Cosine distance from the question vector.

**`err`**

The request failed.

- **`err.reason`: `String`** — Human-readable error description.

## Example

```sh
curl -s -X POST http://127.0.0.1:8080/memory/ask/invoke \
  -H 'Content-Type: application/json' \
  -d '{"question": "What is the capital of France?"}'
# {"ok":{"answer":"Paris","sources":[{"blob_id":"...","text":"Paris is the capital of France","distance":0.05}]}}

# No relevant memories → answer with empty sources
curl -s -X POST http://127.0.0.1:8080/memory/ask/invoke \
  -H 'Content-Type: application/json' \
  -d '{"question": "What is the speed of light?"}'
# {"ok":{"answer":"I don't know","sources":[]}}
```

---

# `xyz.taluslabs.memory.memwal.analyze@1`

Extract discrete facts from a text document and store each fact as an
individual memory. The relayer runs an LLM fact-extraction pass and enqueues
one memory-write job per fact found.

This tool returns immediately after the jobs are enqueued — it does **not**
block on individual Walrus writes. Use `job_count` for downstream monitoring;
use `remember` if you need a confirmed blob ID.

## Input

**`text`: `String`** *(required)*

The text from which to extract and store facts.

**`namespace`: `String`** *(optional)*

Namespace to store the extracted facts in. Uses the `"default"` namespace when
omitted.

**`server_url`: `String`** *(optional)*

Override the relayer URL for this invocation.

## Output Variants & Ports

**`ok`**

Facts were submitted for storage.

- **`ok.job_count`: `u32`** — Number of individual memory-write jobs enqueued.
  Zero means no facts were extracted from the text.

**`err`**

The request failed.

- **`err.reason`: `String`** — Human-readable error description.

## Example

```sh
curl -s -X POST http://127.0.0.1:8080/memory/analyze/invoke \
  -H 'Content-Type: application/json' \
  -d '{"text": "Alice lives in Paris. Bob works at ACME. Paris is in France."}'
# {"ok":{"job_count":3}}

# No facts extracted → job_count is 0
curl -s -X POST http://127.0.0.1:8080/memory/analyze/invoke \
  -H 'Content-Type: application/json' \
  -d '{"text": "..."}'
# {"ok":{"job_count":0}}
```
