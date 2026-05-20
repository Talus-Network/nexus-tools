#
# $ just memwal
#
# Example invocations for the memory-memwal Nexus Tools bundle. Each recipe
# below targets the bundle's local HTTP server (started via `just memwal server`)
# and prints the JSON response.
#
# -----------------------------------------------------------------------------
# Assumptions (apply to every recipe in this module)
# -----------------------------------------------------------------------------
#   * The toolchain assumptions of the workspace hold: `cargo` is on PATH.
#   * `curl` and `jq` are installed. `jq` is used both to pretty-print
#     responses and to build the request bodies safely (no shell-quoting bugs
#     when text contains quotes, braces, or newlines).
#   * The bundle binary is running and reachable at `host` (default
#     127.0.0.1:8080). Start it in a separate terminal:
#         just memwal server
#   * `MEMWAL_DELEGATE_PRIVATE_KEY` is exported in the binary's environment.
#     Without it the binary still boots, but every `/health` and every
#     `/invoke` will return `AuthError::MissingKey`. With an explicitly-set
#     but malformed key the binary refuses to start (process exit 1).
#   * The default relayer points at testnet
#     (https://relayer.staging.memwal.ai) so a delegate key registered on
#     Sui testnet is required; switch to mainnet by setting
#     `MEMWAL_SERVER_URL=https://relayer.memwal.ai` in the binary's
#     environment.

# Host:port the recipes target. Override on the CLI:
#     just memwal --set host 0.0.0.0:9000 health
host := "127.0.0.1:8080"

[private]
_default:
    @just --list memwal

# -----------------------------------------------------------------------------
# Process lifecycle
# -----------------------------------------------------------------------------
#
# The bundle binary is run in the background and its PID is written to
# `target/memwal-server.pid`; stdout/stderr go to `target/memwal-server.log`.
# `server-start` builds the release binary first, spawns it, and polls
# /tools until it answers — so when the recipe returns, the server is ready.

# Single source of truth for the file names under the Cargo workspace's
# target/ directory. Everything else derives its paths from these two plus the
# `_workspace-root` private recipe below.
_pid_subpath := "target/memwal-server.pid"
_log_subpath := "target/memwal-server.log"

# Bash helper: print an array of command tokens as a copy-paste-ready shell
# command line. Tokens that contain only "safe" characters
# (letters, digits, `_`, `/`, `.`, `@`, `:`, `=`, `,`, `+`, `-`) are printed
# verbatim; anything else (whitespace, quotes, braces, `?`, etc.) is wrapped
# in single quotes, with internal single quotes escaped as '\''.
#
# Defined once here as a raw multi-line just string and interpolated into
# every recipe that emits a curl trace — single source of truth for the
# formatting rule.
_print_cmd_fn := '''
_print_cmd() {
    local a
    for a in "$@"; do
        if [[ -z "$a" || "$a" =~ [^A-Za-z0-9_/.@:=,+-] ]]; then
            printf "'%s' " "${a//\'/\'\\\'\'}"
        else
            printf '%s ' "$a"
        fi
    done
    echo
}
'''

# Authoritative anchor for "where the Cargo workspace lives" — asks Cargo
# directly, so it's correct even if the workspace ever moves relative to the
# git or just roots. Defined as a private recipe (not a top-level backticked
# variable) so the cargo startup cost is paid only by recipes that actually
# need the path — not on every `just --list` or tab completion.
[private]
_workspace-root:
    @dirname "$(cargo locate-project --workspace --message-format plain)"

# Build the bundle (release) and start it detached. Polls /tools for up to
# 30 s and exits non-zero if the server did not come up.
#
# Default bind: 127.0.0.1:8080. Override:  just memwal server-start 0.0.0.0:9000
# (when overriding bind, also override `host` so the recall/ask/... recipes
# target the same port:   just memwal --set host 0.0.0.0:9000 server-start)
#
# Env loading:
#   If `tools/memory-memwal/.env` exists, its KEY=VALUE entries are loaded
#   into the binary's environment BEFORE spawning. Variables that are
#   already set in the calling shell take precedence, so an ad-hoc
#   `KEY=foo just memwal server-start` still wins over the file.
#
# Build the release binary, spawn it detached, wait until /tools answers.
server-start bind=host:
    #!/usr/bin/env bash
    set -euo pipefail
    workspace_root="$(just memwal::_workspace-root)"
    pid_file="$workspace_root/{{_pid_subpath}}"
    log_file="$workspace_root/{{_log_subpath}}"
    env_file="$workspace_root/tools/memory-memwal/.env"
    cd "$workspace_root"

    # Load .env defaults without clobbering vars already exported in the
    # calling shell: snapshot current exports, source .env, then replay the
    # snapshot so original values override any reassignments. Variables that
    # only exist in .env survive the replay (they aren't in the snapshot).
    if [[ -f "$env_file" ]]; then
        echo "Loading env defaults from $env_file"
        env_snapshot=$(declare -px | grep '^declare -x ' || true)
        set -a
        # shellcheck disable=SC1090
        source "$env_file"
        set +a
        eval "$env_snapshot"
    fi

    if [[ -f "$pid_file" ]]; then
        existing=$(cat "$pid_file")
        if kill -0 "$existing" 2>/dev/null; then
            echo "memwal server already running (PID $existing)"; exit 1
        fi
        echo "Stale PID file; removing."
        rm -f "$pid_file"
    fi
    echo "Building memory-memwal (release)..."
    cargo +stable build --package memory-memwal --release --quiet
    mkdir -p "$(dirname "$pid_file")"
    BIND_ADDR={{bind}} nohup ./target/release/memory-memwal \
        > "$log_file" 2>&1 &
    pid=$!
    echo "$pid" > "$pid_file"
    # The bind probe address may differ from {{bind}} (e.g. 0.0.0.0 is not
    # routable from a client); always probe via {{host}}.
    for i in $(seq 1 30); do
        if curl -sSf "http://{{host}}/tools" > /dev/null 2>&1; then
            echo "Ready at http://{{host}}  (bind={{bind}}, PID $pid, log $log_file)"
            exit 0
        fi
        sleep 1
    done
    echo "FAIL: server did not answer on http://{{host}}/tools within 30s." >&2
    echo "Tail the log:  just memwal server-logs" >&2
    exit 1

# Read `target/memwal-server.pid`, send SIGTERM, wait up to 5 s, then
# escalate to SIGKILL if still alive. Removes the PID file on success.
#
# Stop the detached server started by `server-start`.
server-stop:
    #!/usr/bin/env bash
    set -euo pipefail
    pid_file="$(just memwal::_workspace-root)/{{_pid_subpath}}"
    if [[ ! -f "$pid_file" ]]; then
        echo "No PID file at $pid_file; nothing to stop."; exit 0
    fi
    pid=$(cat "$pid_file")
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "PID $pid is not alive; clearing stale PID file."
        rm -f "$pid_file"; exit 0
    fi
    kill -TERM "$pid"
    for i in $(seq 1 5); do
        if ! kill -0 "$pid" 2>/dev/null; then
            rm -f "$pid_file"
            echo "Stopped PID $pid."
            exit 0
        fi
        sleep 1
    done
    echo "PID $pid did not exit after SIGTERM; sending SIGKILL." >&2
    kill -KILL "$pid" || true
    rm -f "$pid_file"

# Report whether the PID file is present, the process is alive, and /tools
# answers. Exit code: 0 if fully up, 1 otherwise (suitable for scripts).
#
# Report PID + process liveness + /tools reachability.
server-status:
    #!/usr/bin/env bash
    set -euo pipefail
    pid_file="$(just memwal::_workspace-root)/{{_pid_subpath}}"
    if [[ ! -f "$pid_file" ]]; then
        echo "stopped (no PID file)"; exit 1
    fi
    pid=$(cat "$pid_file")
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "stopped (PID $pid is not alive; PID file is stale)"; exit 1
    fi
    if curl -sSf "http://{{host}}/tools" > /dev/null 2>&1; then
        echo "running (PID $pid, http://{{host}})"
    else
        echo "process alive (PID $pid) but http://{{host}}/tools not answering"
        exit 1
    fi

# `tail -n 50 -F target/memwal-server.log`. Ctrl-C to stop.
#
# Tail the detached server's log.
server-logs:
    tail -n 50 -F "$(just memwal::_workspace-root)/{{_log_subpath}}"

# -----------------------------------------------------------------------------
# Read-only inspection (no auth required, no relayer round-trip)
# -----------------------------------------------------------------------------

# GET /tools — list the registered tools (FQN + path + JSON schemas).
# Always safe to call; does not hit the relayer.
#
# GET /tools — list registered tool FQNs and JSON schemas. No auth.
tools:
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    cmd=(curl --fail-with-body -sS "http://{{host}}/tools")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}" | jq

# -----------------------------------------------------------------------------
# Liveness (auth required; hits the relayer)
# -----------------------------------------------------------------------------

# GET /health — validates the delegate key locally then calls the relayer's
# /health and verifies the reported version matches `MEMWAL_API_VERSION`.
# Returns HTTP 200 on success. Fails with `MissingKey` if the env var is unset.
#
# GET /health — checks delegate key + relayer reachability + version.
health:
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    cmd=(curl --fail-with-body -sS "http://{{host}}/health")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}"

# -----------------------------------------------------------------------------
# Tool invocations
# -----------------------------------------------------------------------------

# POST /memory/remember/invoke — store a single memory.
# Blocks until the memory is durably written to Walrus, then returns the
# blob_id. Suitable when a downstream Nexus DAG vertex needs a confirmed
# write before activating.
#
# Args:
#   text       — required, the text to store
#   namespace  — optional, defaults to "default"
#
# Example:
#   just memwal remember "Paris is the capital of France."
#   just memwal remember "Alice works at ACME" "people"
#
# Store a memory; blocks until written. Returns blob_id.
remember text namespace="default":
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    body=$(jq -nc --arg text {{quote(text)}} --arg ns {{quote(namespace)}} \
        '{text: $text, namespace: $ns}')
    cmd=(curl --fail-with-body -sS -X POST
         "http://{{host}}/memory/remember/invoke"
         -H 'Content-Type: application/json'
         -d "$body")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}" | jq

# POST /memory/recall/invoke — semantic search over stored memories.
# Returns matches ranked by cosine distance (lower = closer).
#
# Args:
#   query      — required, natural-language search query
#   limit      — optional, max results to return (default 5)
#   namespace  — optional, defaults to "default"
#
# Example:
#   just memwal recall "famous landmarks in France" 3
#   just memwal recall "who works at ACME" 10 "people"
#
# Semantic search over stored memories; returns ranked matches.
recall query limit="5" namespace="default":
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    body=$(jq -nc \
        --arg q {{quote(query)}} \
        --argjson limit {{quote(limit)}} \
        --arg ns {{quote(namespace)}} \
        '{query: $q, limit: $limit, namespace: $ns}')
    cmd=(curl --fail-with-body -sS -X POST
         "http://{{host}}/memory/recall/invoke"
         -H 'Content-Type: application/json'
         -d "$body")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}" | jq

# POST /memory/ask/invoke — memory-augmented question answering.
# The relayer retrieves the top-`limit` memories most relevant to the
# question, injects them into an LLM prompt, and returns the answer with
# the source memories used as context.
#
# Args:
#   question   — required, the question to answer
#   limit      — optional, max source memories to inject (default 3)
#   namespace  — optional, defaults to "default"
#
# Example:
#   just memwal ask "Where is the Eiffel Tower?"
#   just memwal ask "Who works at ACME?" 5 "people"
#
# Memory-augmented Q&A; returns LLM answer + source memories.
ask question limit="3" namespace="default":
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    body=$(jq -nc \
        --arg q {{quote(question)}} \
        --argjson limit {{quote(limit)}} \
        --arg ns {{quote(namespace)}} \
        '{question: $q, limit: $limit, namespace: $ns}')
    cmd=(curl --fail-with-body -sS -X POST
         "http://{{host}}/memory/ask/invoke"
         -H 'Content-Type: application/json'
         -d "$body")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}" | jq

# POST /memory/analyze/invoke — extract facts from text and store each as a
# separate memory. Returns the number of memory-write jobs enqueued.
#
# Fire-and-forget: this returns BEFORE individual jobs complete. Use
# `remember` instead when a confirmed blob_id is required for downstream
# DAG vertices to activate.
#
# Args:
#   text       — required, multi-sentence text to extract facts from
#   namespace  — optional, defaults to "default"
#
# Example:
#   just memwal analyze "Alice lives in Paris. Bob works at ACME."
#   just memwal analyze "Carol leads design. Dave handles ops." "team"
#
# Extract facts and enqueue one memory-write per fact. Fire-and-forget.
analyze text namespace="default":
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    body=$(jq -nc --arg text {{quote(text)}} --arg ns {{quote(namespace)}} \
        '{text: $text, namespace: $ns}')
    cmd=(curl --fail-with-body -sS -X POST
         "http://{{host}}/memory/analyze/invoke"
         -H 'Content-Type: application/json'
         -d "$body")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}" | jq

# POST /memory/forget/invoke — delete every memory in a namespace.
# Owner-scoped: only memories owned by the authenticated account are removed.
# Returns the count of memories deleted (zero is a valid success).
#
# Args:
#   namespace  — optional, defaults to "default"
#
# Example:
#   just memwal forget "scratch-pad"
#   just memwal forget                      # clears the default namespace
#
# Delete every memory in a namespace; returns the count removed.
forget namespace="default":
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    body=$(jq -nc --arg ns {{quote(namespace)}} '{namespace: $ns}')
    cmd=(curl --fail-with-body -sS -X POST
         "http://{{host}}/memory/forget/invoke"
         -H 'Content-Type: application/json'
         -d "$body")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}" | jq

# POST /memory/stats/invoke — count + storage_bytes for a namespace.
# Useful as a quota guard before heavy writes (per-account cap is 1 GB).
#
# Args:
#   namespace  — optional, defaults to "default"
#
# Example:
#   just memwal stats "people"
#   just memwal stats                       # stats for the default namespace
#
# Report memory count and storage bytes for a namespace.
stats namespace="default":
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    body=$(jq -nc --arg ns {{quote(namespace)}} '{namespace: $ns}')
    cmd=(curl --fail-with-body -sS -X POST
         "http://{{host}}/memory/stats/invoke"
         -H 'Content-Type: application/json'
         -d "$body")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}" | jq

# POST /memory/remember_bulk/invoke — store up to 20 memories in one batched
# call. The relayer charges weight 10 for the batch vs weight 5 per
# individual /api/remember, so this is 10× more rate-limit-efficient when
# you have multiple items to write at once.
#
# Args:
#   items_json — required, a JSON array of objects:
#                  [{"text": "...", "namespace": "..."}, ...]
#                Namespace is per-item and optional.
#
# Example:
#   just memwal remember-bulk '[{"text":"a"},{"text":"b","namespace":"team"}]'
#
# Store up to 20 memories in one batched call.
remember-bulk items_json:
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    body=$(jq -nc --argjson items {{quote(items_json)}} '{items: $items}')
    cmd=(curl --fail-with-body -sS -X POST
         "http://{{host}}/memory/remember_bulk/invoke"
         -H 'Content-Type: application/json'
         -d "$body")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}" | jq

# -----------------------------------------------------------------------------
# End-to-end smoke test
# -----------------------------------------------------------------------------

# Self-sufficient end-to-end demo: starts the server if it's not already
# running, runs ONE of each tool — /tools, /health, remember, recall, ask,
# analyze, stats, forget — in a fresh timestamp-suffixed namespace, and
# stops the server on exit (only if we were the ones who started it).
#
# `remember_bulk` is intentionally omitted (weight 10 would push the demo
# over the 30/min cap); run it manually with `just memwal remember-bulk`.
#
# Rate-limit budget: the relayer caps signed calls at 30 weighted-
# requests/min per delegate key. The demo's total weight is ~18:
#   health(1) + remember(5) + recall(1) + ask(2) + analyze(5+~2 facts) +
#   stats(1) + forget(1). If you re-run the demo within a minute you may
#   still see HTTP 429 because the sliding window hasn't fully cleared.
#
# Requires: MEMWAL_DELEGATE_PRIVATE_KEY exported in the binary's environment.
# That env var is read at server boot; export it before calling `demo`.
#
# Example:
#   MEMWAL_DELEGATE_PRIVATE_KEY=<hex> just memwal demo
#
# Self-sufficient end-to-end demo: auto-manages the server lifecycle.
demo:
    #!/usr/bin/env bash
    set -euo pipefail

    # Auto-manage the server: start it iff it isn't already running, and
    # tear it down on exit only in that case so a developer-started server
    # is left alone.
    if just memwal::server-status > /dev/null 2>&1; then
        echo "Using already-running server."
    else
        echo "Server not running — starting it now."
        just memwal::server-start
        # `set -e` ensures any failure below still hits this trap.
        trap 'echo; echo "Demo done — stopping the server we started."; just memwal::server-stop' EXIT
    fi
    echo

    # Each wrapper recipe prints the actual curl command it's about to run
    # (single source of truth via a bash array), so we just call them
    # directly — no need for a wrapper echo helper.
    ns="demo-$(date +%s)"
    echo "== namespace: $ns =="
    echo
    just memwal::tools
    echo
    just memwal::health
    echo
    echo
    just memwal::remember "The Eiffel Tower is in Paris, France." "$ns"
    echo
    just memwal::recall "famous landmarks in France" 3 "$ns"
    echo
    just memwal::ask "Where is the Eiffel Tower?" 3 "$ns"
    echo
    just memwal::analyze "Carol leads design. Dave handles ops." "$ns"
    echo
    just memwal::stats "$ns"
    echo
    just memwal::forget "$ns"
