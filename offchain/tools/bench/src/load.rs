//! # `xyz.taluslabs.bench.load@1`
//!
//! Synthetic load tool that waits for [`Input::sleep_ms`], creates
//! [`Input::payload_bytes`] bytes, returns [`Output::Err`] according to
//! [`Input::error_rate`], and copies [`Input::echo`] for vertex chains.

use {
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    rand::Rng as _,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    std::time::{Duration, Instant},
};

const MAX_SLEEP_MS: u64 = 60_000;
// Leave room for the JSON envelope below the 61 440 byte inline data limit.
const MAX_PAYLOAD_BYTES: u64 = 61_000;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// Handler delay before responding, in milliseconds.
    #[serde(default)]
    sleep_ms: u64,
    /// Size of the response padding in bytes.
    #[serde(default)]
    payload_bytes: u64,
    /// Probability from 0.0 through 1.0 of returning [`Output::Err`].
    #[serde(default)]
    error_rate: f64,
    /// Passthrough for chaining vertices with real data flow.
    #[serde(default)]
    echo: i64,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    /// Copies [`Input::echo`] for chains and branches with distinct output ports.
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

    /// Keeps [`NexusTool::timeout`] stress cases short while exercising leader retries.
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

        match tool.invoke(input(0, 0, 1.5, 0)).await {
            Output::Err { reason } => assert!(
                reason.contains("not within"),
                "expected a range validation reason, got: {reason}"
            ),
            Output::Ok { .. } => panic!("error_rate 1.5 must be rejected"),
        }
        match tool.invoke(input(0, 0, -0.5, 0)).await {
            Output::Err { reason } => assert!(
                reason.contains("not within"),
                "expected a range validation reason, got: {reason}"
            ),
            Output::Ok { .. } => panic!("error_rate -0.5 must be rejected"),
        }
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

    #[tokio::test(start_paused = true)]
    async fn accepts_values_exactly_at_the_caps() {
        let tool = BenchLoad::new().await;
        assert!(matches!(
            tool.invoke(input(MAX_SLEEP_MS, 0, 0.0, 0)).await,
            Output::Ok { .. }
        ));
        match tool.invoke(input(0, MAX_PAYLOAD_BYTES, 0.0, 0)).await {
            Output::Ok { payload, .. } => {
                assert_eq!(payload.len(), MAX_PAYLOAD_BYTES as usize)
            }
            Output::Err { reason } => panic!("unexpected err at the payload cap: {reason}"),
        }
    }

    #[test]
    fn registers_a_five_second_timeout() {
        assert_eq!(BenchLoad::timeout(), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn defaults_make_every_port_optional() {
        let parsed: Input = serde_json::from_str(r#"{"echo": 7}"#).unwrap();
        assert_eq!(parsed.sleep_ms, 0);
        assert_eq!(parsed.payload_bytes, 0);
        assert_eq!(parsed.error_rate, 0.0);
        assert_eq!(parsed.echo, 7);

        let tool = BenchLoad::new().await;
        assert!(matches!(tool.health().await, Ok(StatusCode::OK)));
    }
}
