//! # `xyz.taluslabs.bench.load@1`
//!
//! Synthetic load tool: sleeps `sleep_ms`, pads the output with
//! `payload_bytes` bytes, errors with probability `error_rate`, and passes
//! `echo` through for vertex chaining.

use {
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    rand::Rng as _,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    std::time::{Duration, Instant},
};

const MAX_SLEEP_MS: u64 = 60_000;
const MAX_PAYLOAD_BYTES: u64 = 262_144;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// Handler delay before responding, in milliseconds.
    #[serde(default)]
    sleep_ms: u64,
    /// Size of the response padding in bytes.
    #[serde(default)]
    payload_bytes: u64,
    /// Probability in [0, 1] of returning the `err` variant.
    #[serde(default)]
    error_rate: f64,
    /// Passthrough for chaining vertices with real data flow.
    #[serde(default)]
    echo: i64,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// `b0..b3` are copies of `echo`: a DAG output port feeds at most one
    /// edge, so fanning one vertex out to w branches needs w ports on one
    /// variant. Chains wire `echo`; fan-out shapes wire `b0..b{w-1}`.
    Ok {
        echo: i64,
        payload: String,
        b0: i64,
        b1: i64,
        b2: i64,
        b3: i64,
    },
    Err {
        reason: String,
    },
}

pub(crate) struct BenchLoad;

impl NexusTool for BenchLoad {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        Self
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.bench.load@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/load"
    }

    fn description() -> &'static str {
        "Synthetic load tool: configurable delay, payload size, and error rate."
    }

    /// 5 s registered timeout: the walk window becomes 10 s and hard
    /// expiry 20 s (registered + 5 s buffer, then 2x) — half the
    /// default-tool numbers, keeping timeout-path stress runs short. The
    /// leader's per-attempt HTTP wait is this same value, so sleeps above
    /// it exercise the timeout/retry/terminal-eval path (s5-sleep-8s).
    fn timeout() -> Duration {
        Duration::from_secs(5)
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        let started = Instant::now();

        if !input.error_rate.is_finite() || !(0.0..=1.0).contains(&input.error_rate) {
            return Output::Err {
                reason: format!("error_rate {} is not within [0, 1]", input.error_rate),
            };
        }
        if input.sleep_ms > MAX_SLEEP_MS {
            return Output::Err {
                reason: format!("sleep_ms {} exceeds the {MAX_SLEEP_MS} cap", input.sleep_ms),
            };
        }
        if input.payload_bytes > MAX_PAYLOAD_BYTES {
            return Output::Err {
                reason: format!(
                    "payload_bytes {} exceeds the {MAX_PAYLOAD_BYTES} cap",
                    input.payload_bytes
                ),
            };
        }

        if input.sleep_ms > 0 {
            tokio::time::sleep(Duration::from_millis(input.sleep_ms)).await;
        }

        let errored = input.error_rate > 0.0 && rand::thread_rng().gen::<f64>() < input.error_rate;
        let output = if errored {
            Output::Err {
                reason: format!("synthetic error (error_rate {})", input.error_rate),
            }
        } else {
            Output::Ok {
                echo: input.echo,
                payload: "x".repeat(input.payload_bytes as usize),
                b0: input.echo,
                b1: input.echo,
                b2: input.echo,
                b3: input.echo,
            }
        };

        log::info!(
            "bench.load elapsed_ms={} sleep_ms={} payload_bytes={} errored={}",
            started.elapsed().as_millis(),
            input.sleep_ms,
            input.payload_bytes,
            errored
        );
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(sleep_ms: u64, payload_bytes: u64, error_rate: f64, echo: i64) -> Input {
        Input {
            sleep_ms,
            payload_bytes,
            error_rate,
            echo,
        }
    }

    #[tokio::test]
    async fn passes_echo_through_all_ports_and_pads_the_payload() {
        let tool = BenchLoad::new().await;
        let output = tool.invoke(input(0, 16, 0.0, 42)).await;
        match output {
            Output::Ok {
                echo,
                payload,
                b0,
                b1,
                b2,
                b3,
            } => {
                assert_eq!(echo, 42);
                assert_eq!(payload.len(), 16);
                // Fan-out ports mirror echo so every branch gets real data.
                assert_eq!((b0, b1, b2, b3), (42, 42, 42, 42));
            }
            Output::Err { reason } => panic!("unexpected err: {reason}"),
        }
    }

    #[tokio::test]
    async fn sleeps_at_least_the_requested_delay() {
        let tool = BenchLoad::new().await;
        let started = std::time::Instant::now();
        let output = tool.invoke(input(50, 0, 0.0, 0)).await;
        assert!(matches!(output, Output::Ok { .. }));
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "invoke returned before the requested sleep elapsed"
        );
    }

    #[tokio::test]
    async fn error_rate_one_always_errs_and_zero_never_does() {
        let tool = BenchLoad::new().await;
        for _ in 0..8 {
            assert!(matches!(
                tool.invoke(input(0, 0, 1.0, 0)).await,
                Output::Err { .. }
            ));
            assert!(matches!(
                tool.invoke(input(0, 0, 0.0, 0)).await,
                Output::Ok { .. }
            ));
        }
    }

    #[tokio::test]
    async fn rejects_out_of_range_inputs() {
        let tool = BenchLoad::new().await;
        assert!(matches!(
            tool.invoke(input(0, 0, 1.5, 0)).await,
            Output::Err { .. }
        ));
        assert!(matches!(
            tool.invoke(input(0, 0, f64::NAN, 0)).await,
            Output::Err { .. }
        ));
        assert!(matches!(
            tool.invoke(input(MAX_SLEEP_MS + 1, 0, 0.0, 0)).await,
            Output::Err { .. }
        ));
        assert!(matches!(
            tool.invoke(input(0, MAX_PAYLOAD_BYTES + 1, 0.0, 0)).await,
            Output::Err { .. }
        ));
    }

    #[tokio::test]
    async fn defaults_make_every_port_optional() {
        // DAG interior vertices receive only `echo` via an edge; the other
        // ports must deserialize from an absent field.
        let parsed: Input = serde_json::from_str(r#"{"echo": 7}"#).unwrap();
        assert_eq!(parsed.sleep_ms, 0);
        assert_eq!(parsed.payload_bytes, 0);
        assert_eq!(parsed.error_rate, 0.0);
        assert_eq!(parsed.echo, 7);

        let tool = BenchLoad::new().await;
        assert!(matches!(tool.health().await, Ok(StatusCode::OK)));
    }
}
