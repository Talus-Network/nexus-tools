//! `xyz.taluslabs.llm.openai.openai-completion@1`
//!
//! OpenAI legacy text completions API.

use {
    async_openai::{
        config::OpenAIConfig,
        error::OpenAIError,
        types::CreateCompletionRequestArgs,
        Client,
    },
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    std::sync::OnceLock,
};

// ── config ────────────────────────────────────────────────────────────────────
//
// OPENAI_API_KEY is read once at startup and cached. HEALTHCHECK_URL is
// optional and read from env at call time so tests can override via
// std::env::set_var without running validate_config.

static OPENAI_API_KEY: OnceLock<String> = OnceLock::new();

/// Reads and caches all required env vars. Called from main() before bootstrap!.
pub(crate) fn validate_config() {
    OPENAI_API_KEY
        .set(load_required("OPENAI_API_KEY"))
        .expect("validate_config called twice");
}

fn load_required(name: &str) -> String {
    match std::env::var(name) {
        Ok(v) => {
            log::debug!(target: "openai_completion", "env var {name} loaded");
            v
        }
        Err(_) => {
            log::error!(target: "openai_completion", "fatal: required env var {name} is not set");
            std::process::exit(1);
        }
    }
}

fn openai_api_key() -> &'static str {
    OPENAI_API_KEY
        .get()
        .expect("validate_config must run before any accessor")
}

fn healthcheck_url() -> String {
    std::env::var("HEALTHCHECK_URL")
        .unwrap_or_else(|_| "https://status.openai.com/api/v2/status.json".to_string())
}

// ── constants ─────────────────────────────────────────────────────────────────

const DEFAULT_MODEL: &str = "gpt-3.5-turbo-instruct";
const DEFAULT_MAX_TOKENS: u32 = 512;
const DEFAULT_TEMPERATURE: f32 = 1.0;

// ── types ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// Text prompt to complete.
    prompt: String,
    /// Model to use. Defaults to gpt-3.5-turbo-instruct.
    #[serde(default = "default_model")]
    model: String,
    /// Maximum number of tokens to generate. Defaults to 512.
    #[serde(default = "default_max_tokens")]
    max_tokens: u32,
    /// Sampling temperature (0.0–2.0). Defaults to 1.0.
    #[serde(default = "default_temperature")]
    temperature: f32,
    /// Stop sequences — generation halts when any is encountered (up to 4).
    #[serde(default)]
    stop: Option<Vec<String>>,
    /// Text to append after the completion (fill-in-the-middle suffix).
    #[serde(default)]
    suffix: Option<String>,
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}
fn default_max_tokens() -> u32 {
    DEFAULT_MAX_TOKENS
}
fn default_temperature() -> f32 {
    DEFAULT_TEMPERATURE
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    Ok {
        completion: String,
        model: String,
        finish_reason: String,
        prompt_tokens: u32,
        completion_tokens: u32,
    },
    /// The upstream OpenAI API returned an error that is not auth or rate-limit.
    ErrUpstream { reason: String },
    /// The API key is invalid or authentication failed.
    ErrAuth { reason: String },
    /// The OpenAI rate limit was exceeded; callers should back off and retry.
    ErrRateLimited { reason: String },
}

pub(crate) struct OpenaiCompletion;

// ── impl ──────────────────────────────────────────────────────────────────────

impl NexusTool for OpenaiCompletion {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        Self
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.llm.openai.openai-completion@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/openai_completion"
    }

    fn description() -> &'static str {
        "OpenAI legacy text completions API."
    }

    fn timeout() -> std::time::Duration {
        // Completions can be slow for long outputs.
        std::time::Duration::from_secs(60)
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        check_health(&healthcheck_url()).await
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        invoke_impl(input, openai_api_key(), None).await
    }
}

// ── internals (pub for testing) ───────────────────────────────────────────────

/// Checks the OpenAI status page. Accepts `url` as a parameter so tests can
/// inject a mock URL without needing validate_config to run.
pub(crate) async fn check_health(url: &str) -> AnyResult<StatusCode> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;

    let raw = match client.get(url).send().await {
        Ok(resp) => resp.text().await?,
        Err(e) => {
            log::warn!(target: "openai_completion", "health check request failed: {e}");
            return Ok(StatusCode::SERVICE_UNAVAILABLE);
        }
    };

    log::debug!(target: "openai_completion", "health check response: {raw}");

    let body: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            log::error!(target: "openai_completion", "health check parse error: {e}");
            return Ok(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let indicator = body["status"]["indicator"].as_str().unwrap_or("major");
    if matches!(indicator, "none" | "minor") {
        Ok(StatusCode::OK)
    } else {
        log::warn!(target: "openai_completion", "OpenAI status indicator={indicator}");
        Ok(StatusCode::SERVICE_UNAVAILABLE)
    }
}

/// Core logic, parameterised over `api_key` and optional `api_base` so tests
/// can inject mock credentials and server URLs without running validate_config.
pub(crate) async fn invoke_impl(input: Input, api_key: &str, api_base: Option<&str>) -> Output {
    log::debug!(
        target: "openai_completion",
        "invoke called: model={:?}, max_tokens={}, temperature={}",
        input.model, input.max_tokens, input.temperature
    );

    let mut cfg = OpenAIConfig::new().with_api_key(api_key);
    if let Some(base) = api_base {
        cfg = cfg.with_api_base(base);
    }
    let client = Client::with_config(cfg);

    let mut builder = CreateCompletionRequestArgs::default();
    builder
        .model(&input.model)
        .prompt(input.prompt)
        .max_tokens(input.max_tokens as u16)
        .temperature(input.temperature);

    if let Some(stops) = input.stop {
        use async_openai::types::Stop;
        let stop = match stops.len() {
            0 => None,
            1 => Some(Stop::String(stops.into_iter().next().unwrap())),
            _ => Some(Stop::StringArray(stops)),
        };
        if let Some(stop) = stop {
            builder.stop(stop);
        }
    }

    if let Some(suffix) = input.suffix {
        builder.suffix(suffix);
    }

    let request = match builder.build() {
        Ok(r) => r,
        Err(err) => {
            log::warn!(target: "openai_completion", "failed to build completion request: {err}");
            return Output::ErrUpstream {
                reason: format!("request build error: {err}"),
            };
        }
    };

    match client.completions().create(request).await {
        Ok(response) => {
            let choice = match response.choices.first() {
                Some(c) => c,
                None => {
                    log::warn!(target: "openai_completion", "no choices in response");
                    return Output::ErrUpstream {
                        reason: "no choices returned from OpenAI API".to_string(),
                    };
                }
            };

            // Convert FinishReason enum to its serialised string form.
            let finish_reason = serde_json::to_value(choice.finish_reason)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string());

            let prompt_tokens = response
                .usage
                .as_ref()
                .map(|u| u.prompt_tokens)
                .unwrap_or(0);
            let completion_tokens = response
                .usage
                .as_ref()
                .map(|u| u.completion_tokens)
                .unwrap_or(0);

            log::info!(
                target: "openai_completion",
                "ok: model={}, finish_reason={finish_reason}, tokens={prompt_tokens}+{completion_tokens}",
                response.model
            );

            Output::Ok {
                completion: choice.text.clone(),
                model: response.model.clone(),
                finish_reason,
                prompt_tokens,
                completion_tokens,
            }
        }
        Err(OpenAIError::ApiError(api_err)) => {
            let code = api_err.code.as_deref().unwrap_or("");
            let type_ = api_err.r#type.as_deref().unwrap_or("");
            log::warn!(
                target: "openai_completion",
                "api error: type={type_:?} code={code:?} message={}",
                api_err.message
            );
            if code == "invalid_api_key" || type_ == "authentication_error" {
                Output::ErrAuth {
                    reason: api_err.message,
                }
            } else if code.contains("rate_limit") || type_ == "tokens" {
                Output::ErrRateLimited {
                    reason: api_err.message,
                }
            } else {
                Output::ErrUpstream {
                    reason: api_err.message,
                }
            }
        }
        Err(err) => {
            log::warn!(target: "openai_completion", "upstream error: {err}");
            Output::ErrUpstream {
                reason: err.to_string(),
            }
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use {super::*, mockito::Server, serde_json::json};

    fn make_input(prompt: &str) -> Input {
        Input {
            prompt: prompt.to_string(),
            model: "gpt-3.5-turbo-instruct".to_string(),
            max_tokens: 50,
            temperature: 1.0,
            stop: None,
            suffix: None,
        }
    }

    fn completion_body(text: &str) -> String {
        json!({
            "id": "cmpl-test",
            "object": "text_completion",
            "created": 1234567890,
            "model": "gpt-3.5-turbo-instruct",
            "choices": [
                {
                    "text": text,
                    "index": 0,
                    "logprobs": null,
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 7,
                "total_tokens": 12
            }
        })
        .to_string()
    }

    fn api_error_body(code: &str, type_: &str, message: &str) -> String {
        json!({
            "error": {
                "message": message,
                "type": type_,
                "code": code
            }
        })
        .to_string()
    }

    /// Success path: API returns a valid completion.
    #[tokio::test]
    async fn invoke_returns_ok() {
        let mut server = Server::new_async().await;
        let api_base = format!("{}/v1", server.url());

        let _mock = server
            .mock("POST", "/v1/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(completion_body(" Hello, world!"))
            .create_async()
            .await;

        let output = invoke_impl(make_input("Say hello"), "test-key", Some(&api_base)).await;
        let Output::Ok {
            completion,
            prompt_tokens,
            completion_tokens,
            ..
        } = output
        else {
            panic!("expected Ok, got {output:?}");
        };
        assert_eq!(completion, " Hello, world!");
        assert_eq!(prompt_tokens, 5);
        assert_eq!(completion_tokens, 7);
    }

    /// Auth failure: invalid API key returns ErrAuth.
    #[tokio::test]
    async fn invoke_returns_err_auth() {
        let mut server = Server::new_async().await;
        let api_base = format!("{}/v1", server.url());

        let _mock = server
            .mock("POST", "/v1/completions")
            .with_status(401)
            .with_header("content-type", "application/json")
            .with_body(api_error_body(
                "invalid_api_key",
                "invalid_request_error",
                "Incorrect API key provided",
            ))
            .create_async()
            .await;

        let output = invoke_impl(make_input("test"), "bad-key", Some(&api_base)).await;
        assert!(
            matches!(output, Output::ErrAuth { .. }),
            "expected ErrAuth, got {output:?}"
        );
    }

    /// Rate limit: async_openai retries 429s for up to 15 min (default backoff), so the
    /// mock uses 400 to avoid that. The classification is based on the error code field,
    /// not the HTTP status, so this still exercises the ErrRateLimited branch.
    #[tokio::test]
    async fn invoke_returns_err_rate_limited() {
        let mut server = Server::new_async().await;
        let api_base = format!("{}/v1", server.url());

        let _mock = server
            .mock("POST", "/v1/completions")
            .with_status(400) // 400 to prevent async_openai's built-in 429 retry loop
            .with_header("content-type", "application/json")
            .with_body(api_error_body(
                "rate_limit_exceeded",
                "requests",
                "Rate limit reached for requests",
            ))
            .create_async()
            .await;

        let output = invoke_impl(make_input("test"), "test-key", Some(&api_base)).await;
        assert!(
            matches!(output, Output::ErrRateLimited { .. }),
            "expected ErrRateLimited, got {output:?}"
        );
    }

    /// Generic upstream error: 500 returns ErrUpstream.
    #[tokio::test]
    async fn invoke_returns_err_upstream_on_server_error() {
        let mut server = Server::new_async().await;
        let api_base = format!("{}/v1", server.url());

        let _mock = server
            .mock("POST", "/v1/completions")
            .with_status(500)
            .with_header("content-type", "application/json")
            .with_body(api_error_body(
                "internal",
                "server_error",
                "The server had an error processing your request",
            ))
            .create_async()
            .await;

        let output = invoke_impl(make_input("test"), "test-key", Some(&api_base)).await;
        assert!(
            matches!(output, Output::ErrUpstream { .. }),
            "expected ErrUpstream, got {output:?}"
        );
    }

    /// Health check: "none" indicator → 200 OK.
    #[tokio::test]
    async fn health_returns_ok_for_none_indicator() {
        let mut server = Server::new_async().await;
        let url = format!("{}/api/v2/status.json", server.url());

        let _mock = server
            .mock("GET", "/api/v2/status.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({"status": {"indicator": "none", "description": "All Systems Operational"}})
                    .to_string(),
            )
            .create_async()
            .await;

        let result = check_health(&url).await.unwrap();
        assert_eq!(result, StatusCode::OK);
    }

    /// Health check: "minor" degradation → 200 OK (acceptable).
    #[tokio::test]
    async fn health_returns_ok_for_minor_indicator() {
        let mut server = Server::new_async().await;
        let url = format!("{}/api/v2/status.json", server.url());

        let _mock = server
            .mock("GET", "/api/v2/status.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({"status": {"indicator": "minor", "description": "Minor Degradation"}})
                    .to_string(),
            )
            .create_async()
            .await;

        let result = check_health(&url).await.unwrap();
        assert_eq!(result, StatusCode::OK);
    }

    /// Health check: "major" outage → 503 Service Unavailable.
    #[tokio::test]
    async fn health_returns_service_unavailable_for_major_incident() {
        let mut server = Server::new_async().await;
        let url = format!("{}/api/v2/status.json", server.url());

        let _mock = server
            .mock("GET", "/api/v2/status.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({"status": {"indicator": "major", "description": "Major Outage"}})
                    .to_string(),
            )
            .create_async()
            .await;

        let result = check_health(&url).await.unwrap();
        assert_eq!(result, StatusCode::SERVICE_UNAVAILABLE);
    }
}
