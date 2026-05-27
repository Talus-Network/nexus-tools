#!/usr/bin/env bash
# Smoke-test runner for the openai-completion Nexus Tool.
#
# Usage:
#   ./test.sh start [--port N]   build and start the server; print curl examples
#   ./test.sh stop  [--port N]   stop the server
#   ./test.sh run   [--port N]   start, send one sample request, stop
#   ./test.sh dev   [--port N]   start and stream logs; Ctrl+C stops the server
set -euo pipefail

TOOL_NAME="openai-completion"
TOOL_PATH="openai_completion"
WORKSPACE_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
SAMPLE_JSON='{"prompt": "The quick brown fox"}'
DEFAULT_PORT=8080

SUBCMD="${1:-}"
shift || true
PORT="$DEFAULT_PORT"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --port) PORT="$2"; shift 2 ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

RUNDIR="${TMPDIR:-/tmp}/${USER:-nobody}-${TOOL_NAME}-${PORT}"
PID_FILE="${RUNDIR}.pid"
LOG_FILE="${RUNDIR}.log"

# ── helpers ───────────────────────────────────────────────────────────────────

build() {
    echo "► Building $TOOL_NAME (first run may take a few minutes)..."
    if ! (cd "$WORKSPACE_DIR" && cargo +stable build --package "$TOOL_NAME") \
            >"$LOG_FILE" 2>&1; then
        echo "  Build failed. Last lines from $LOG_FILE:" >&2
        tail -20 "$LOG_FILE" >&2
        exit 1
    fi
    echo "  Build complete."
}

start_server() {
    if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
        echo "Server already running (PID $(cat "$PID_FILE"))."
        return
    fi
    build
    local binary="$WORKSPACE_DIR/target/debug/$TOOL_NAME"
    if [[ ! -x "$binary" ]]; then
        echo "  Binary not found at $binary" >&2; exit 1
    fi
    echo "► Starting server on port $PORT..."
    BIND_ADDR="127.0.0.1:${PORT}" "$binary" >>"$LOG_FILE" 2>&1 &
    echo $! >"$PID_FILE"
    echo "► Waiting for /health..."
    for i in $(seq 1 50); do
        if curl -sf "http://localhost:${PORT}/health" >/dev/null 2>&1; then
            echo "  Ready. PID: $(cat "$PID_FILE")  Logs: $LOG_FILE"; return
        fi
        if ! kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
            echo "  Server exited prematurely. Last lines from $LOG_FILE:" >&2
            tail -20 "$LOG_FILE" >&2
            rm -f "$PID_FILE"; exit 1
        fi
        sleep 0.2
    done
    echo "  Timed out waiting for server. Last lines from $LOG_FILE:" >&2
    tail -20 "$LOG_FILE" >&2
    stop_server; exit 1
}

stop_server() {
    if [[ ! -f "$PID_FILE" ]]; then
        echo "No PID file found — server may not be running."; return
    fi
    local pid; pid=$(cat "$PID_FILE")
    if kill -0 "$pid" 2>/dev/null; then
        echo "► Stopping server (PID $pid)..."
        kill "$pid"
    else
        echo "Process $pid not found — cleaning up PID file."
    fi
    rm -f "$PID_FILE"
}

print_curl_examples() {
    local base="http://localhost:${PORT}"
    echo ""
    echo "── curl examples ────────────────────────────────────────────────────────────"
    echo ""
    echo "  Invoke:"
    echo "    curl -s -X POST ${base}/${TOOL_PATH}/invoke \\"
    echo "      -H 'Content-Type: application/json' \\"
    printf "      -d '%s' | jq .\n" "$SAMPLE_JSON"
    echo ""
    echo "  Health:"
    echo "    curl -s ${base}/health"
    echo ""
    echo "─────────────────────────────────────────────────────────────────────────────"
    echo ""
}

send_sample_request() {
    echo "► POST /${TOOL_PATH}/invoke"
    local response
    response=$(curl -s -X POST "http://localhost:${PORT}/${TOOL_PATH}/invoke" \
        -H "Content-Type: application/json" \
        -d "$SAMPLE_JSON")
    if command -v jq >/dev/null 2>&1; then
        echo "$response" | jq .
    else
        echo "$response"
    fi
}

dev_mode() {
    start_server
    print_curl_examples
    local server_pid
    server_pid=$(cat "$PID_FILE")
    echo "► Streaming logs (Ctrl+C to stop server)..."
    # Prefer GNU tail --pid (exits automatically when the server process dies).
    # gtail = GNU tail via Homebrew on macOS; plain tail on Linux is usually GNU.
    local tail_bin=""
    if command -v gtail >/dev/null 2>&1; then
        tail_bin="gtail"
    elif tail --help 2>/dev/null | grep -q -- '--pid'; then
        tail_bin="tail"
    fi
    if [[ -n "$tail_bin" ]]; then
        trap 'stop_server; exit 0' INT TERM
        "$tail_bin" -f --pid="$server_pid" "$LOG_FILE"
    else
        # BSD tail (macOS without gtail): background tail + poll loop
        tail -f "$LOG_FILE" &
        local tail_pid=$!
        trap 'kill "$tail_pid" 2>/dev/null; stop_server; exit 0' INT TERM
        while kill -0 "$server_pid" 2>/dev/null; do
            sleep 0.5
        done
        kill "$tail_pid" 2>/dev/null
    fi
    echo "  Server stopped."
    stop_server
}

# ── dispatch ──────────────────────────────────────────────────────────────────

case "$SUBCMD" in
    start) start_server; print_curl_examples ;;
    stop)  stop_server ;;
    run)   start_server; send_sample_request; stop_server ;;
    dev)   dev_mode ;;
    *)     echo "Usage: $0 {start|stop|run|dev} [--port N]" >&2; exit 1 ;;
esac
