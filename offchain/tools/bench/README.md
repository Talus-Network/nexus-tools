# `xyz.taluslabs.bench.load@1`

This internal tool provides deterministic load for Nexus stress campaigns. It
is not a production tool. Its manifest prevents production deployment and
product documentation.

Input ports:

- `sleep_ms`: delay before the handler responds, at most 60 000 milliseconds.
- `payload_bytes`: size of the `payload` output, at most 61 000 bytes. This is
  below the protocol limit of 61 440 inline bytes, so a valid request does not
  enter the leader `output_serialization_error` path.
- `error_rate`: probability from 0.0 through 1.0 of returning the `err` variant.
- `echo`: `i64` value copied to the `echo` output for vertex chains.

The `ok` variant also copies `echo` to `b0` through `b3`. A DAG output port can
feed one edge, so a branch needs one output port for each destination. Chains
use `echo`; fanout shapes use `b0` through `b3`.

The tool logs elapsed time, requested delay, payload size, and error outcome
for every request because Nexus Toolkit has no Prometheus endpoint.

The registered timeout is 5 seconds. On chain, a walk timeout is the registered
timeout plus a 5 second leader evaluation buffer. Automatic abort is available
between one and two timeout windows, and hard expiry occurs after two windows.
For this tool those boundaries are 10 and 20 seconds.

The leader waits for the registered 5 second timeout on each of at most four
attempts. A delay sweep therefore has two useful regions: below the tool
timeout, where the request completes, and above it, where every attempt times
out before terminal evaluation. Longer delays do not expose another behavior.
