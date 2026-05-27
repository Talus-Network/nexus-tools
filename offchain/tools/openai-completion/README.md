# `xyz.taluslabs.llm.openai.openai-completion@1`

OpenAI legacy text completions API.

Calls `POST /v1/completions` with a plain-text prompt and returns the generated
text. Use `llm-openai-chat-completion` for the chat-style messages API.

The API key is supplied via the `OPENAI_API_KEY` environment variable — never as
an input port.

## Input

**`prompt`: [`String`]** *(required)*

The text prompt to complete.

**`model`: [`String`]** *(default: `"gpt-3.5-turbo-instruct"`)*

The OpenAI model to use. Supported instruct-tuned models include
`gpt-3.5-turbo-instruct`, `davinci-002`, and `babbage-002`.

**`max_tokens`: [`u32`]** *(default: `512`)*

Maximum number of tokens to generate. Capped at 16384 (model-dependent upper
bounds still apply).

**`temperature`: [`f32`]** *(default: `1.0`)*

Sampling temperature in the range 0.0–2.0. Higher values produce more varied
output; lower values are more deterministic.

**`stop`: [`Option<Vec<String>>`]** *(default: none)*

Up to 4 stop sequences. Generation halts when any sequence is encountered.

**`suffix`: [`Option<String>`]** *(default: none)*

Text to insert after the completion (fill-in-the-middle). Supported by select
models only.

## Output Variants & Ports

**`ok`**

The completion succeeded.

- **`ok.completion`: [`String`]** — Generated text.
- **`ok.model`: [`String`]** — Model that produced the response.
- **`ok.finish_reason`: [`String`]** — Why generation stopped (`"stop"`, `"length"`, etc.).
- **`ok.prompt_tokens`: [`u32`]** — Tokens consumed by the prompt.
- **`ok.completion_tokens`: [`u32`]** — Tokens generated.

**`err_upstream`**

The OpenAI API returned an error that is not an auth or rate-limit failure.

- **`err_upstream.reason`: [`String`]** — Human-readable error message.

**`err_auth`**

The API key was rejected by OpenAI.

- **`err_auth.reason`: [`String`]** — Human-readable error message.

**`err_rate_limited`**

The request was rejected because the OpenAI rate limit was exceeded.

- **`err_rate_limited.reason`: [`String`]** — Human-readable error message.
