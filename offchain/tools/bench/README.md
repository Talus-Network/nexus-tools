# `xyz.taluslabs.bench.load@1`

Synthetic load tool for Nexus stress campaigns. Input ports:

- `sleep_ms` — handler delay before responding (max 60 000).
- `payload_bytes` — size of the `payload` output padding (max 61 000, kept
  under the protocol's 61 440-byte inline-data cap so the output never hits
  the leader's `output_serialization_error` path).
- `error_rate` — probability in [0, 1] of returning the `err` variant.
- `echo` — i64 passthrough to the `echo` output port, for chaining vertices
  with real data flow.

The `ok` variant also carries `b0..b3` (copies of `echo`): a DAG output
port may feed only one edge, so fanning one vertex out to w branches needs
w ports on the same variant. Chains use `echo`; fan-out shapes use `b0..b3`.

The tool logs one line per request (elapsed, sleep, payload size, errored)
because toolkit-rust exposes no Prometheus endpoint.

The registered timeout is 5 s. On-chain, a walk's timeout window is the
registered timeout plus a 5 s leader-evaluation buffer, the auto-abort band
is (window, 2x window), and hard expiry is 2x window
(tool_registry.move walk_timeout_ms_for_runtime_vertex; execution.move
is_active_walk_expired) — 10 s / 20 s here, half the default-tool numbers.
The LEADER's per-attempt HTTP wait is the same registered 5 s, retried up
to 4 attempts, so a sleep_ms sweep has exactly two meaningful points:
below the tool timeout (clean) and above it (every attempt times out,
then the terminal-eval failure path). Longer sleeps change nothing the
tool can observe.
