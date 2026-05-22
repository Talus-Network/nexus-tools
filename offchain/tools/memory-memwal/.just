# Example invocations for the memory-memwal bundle. Recipes target the
# bundle's local HTTP server and pretty-print responses via `jq`. Requires
# `curl`, `jq`, `cargo` on PATH and `MEMWAL_DELEGATE_PRIVATE_KEY` exported
# in the binary's environment. Default relayer is testnet; set
# `MEMWAL_SERVER_URL=https://relayer.memwal.ai` for mainnet.

# Host:port the recipes target. Override on the CLI:
#     just memwal --set host 0.0.0.0:9000 health
host := "127.0.0.1:8080"

[private]
_default:
    @just --list memwal

# --- Process lifecycle ---
# The server runs detached; PID at target/memwal-server.pid, logs at
# target/memwal-server.log. server-start blocks until /tools answers.

_pid_subpath := "target/memwal-server.pid"
_log_subpath := "target/memwal-server.log"

# Print an array of tokens as a copy-paste-ready shell command. Quote-only-
# when-needed: single-quote any token that contains characters outside
# `[A-Za-z0-9_/.@:=,+-]`. Single source of truth for command tracing.
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

# `cargo locate-project --workspace` is the authoritative anchor. Defined as
# a private recipe (not a backticked variable) so the cargo invocation only
# runs for recipes that need the path, not for every `just --list`.
[private]
_workspace-root:
    @dirname "$(cargo locate-project --workspace --message-format plain)"

# Override bind:  just memwal server-start 0.0.0.0:9000
# (also set host: just memwal --set host 0.0.0.0:9000 server-start)
# Loads tools/memory-memwal/.env before spawning; existing shell exports win.
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
    # If the binary exits before answering (e.g. FATAL on malformed
    # MEMWAL_DELEGATE_PRIVATE_KEY), break early and dump the log.
    for i in $(seq 1 30); do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "FAIL: server PID $pid exited during startup. Last log lines:" >&2
            tail -n 30 "$log_file" >&2 || true
            rm -f "$pid_file"
            exit 1
        fi
        if curl -sSf "http://{{host}}/tools" > /dev/null 2>&1; then
            echo "Ready at http://{{host}}  (bind={{bind}}, PID $pid, log $log_file)"
            exit 0
        fi
        sleep 1
    done
    echo "FAIL: server did not answer on http://{{host}}/tools within 30s." >&2
    echo "Last log lines:" >&2
    tail -n 30 "$log_file" >&2 || true
    exit 1

# Stop the detached server: SIGTERM, wait 5 s, escalate to SIGKILL.
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

# PID + process liveness + /tools reachability. Exit 0 if fully up.
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

# Tail the detached server's log (`tail -n 50 -F`).
server-logs:
    tail -n 50 -F "$(just memwal::_workspace-root)/{{_log_subpath}}"

# --- Read-only inspection (no auth, no relayer round-trip) ---

# GET /tools — list registered tool FQNs and JSON schemas. No auth.
tools:
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    cmd=(curl --fail-with-body -sS "http://{{host}}/tools")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}" | jq

# --- Liveness (auth required; hits the relayer) ---

# GET /health — checks delegate key + relayer reachability + version match.
health:
    #!/usr/bin/env bash
    set -euo pipefail
    {{_print_cmd_fn}}
    cmd=(curl --fail-with-body -sS "http://{{host}}/health")
    _print_cmd "${cmd[@]}"
    "${cmd[@]}"

# --- Tool invocations ---

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

# Delete every memory in a namespace; returns the count removed (owner-scoped).
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

# Memory count + storage bytes for a namespace (quota guard; 1 GB per account).
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

# items_json: JSON array of `{"text": "...", "namespace": "..."}` (namespace optional per item).
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

# --- End-to-end smoke test ---

# remember_bulk is omitted (weight 10 would push the demo over the 30/min cap).
# Total signed-request weight ~18 — within the cap, but back-to-back re-runs
# may 429 because the sliding window hasn't cleared yet.
# End-to-end demo: auto-manages the server, runs one of each tool.
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
