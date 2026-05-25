#
# $ just tools
#
# Commands related to native Nexus Tools.
#

import '../helpers/helpers.just'

[private]
_default:
    @just --list tools

# Build all native Nexus Tools
build: _check-cargo
    cargo +stable build --package math --release
    cargo +stable build --package llm-openai-chat-completion --release
    cargo +stable build --package payments-stripe --release

# Check all native Nexus Tools
check: _check-cargo
    cargo +stable check --package math
    cargo +stable check --package llm-openai-chat-completion
    cargo +stable check --package payments-stripe

# Run all tests in all native Nexus Tools
test: _check-cargo
    cargo +stable test --package math
    cargo +stable test --package llm-openai-chat-completion
    cargo +stable test --package payments-stripe

# Run rustfmt on all native Nexus Tools
fmt-check: _check-cargo
    #!/usr/bin/env bash

    nightly="$(just helpers::get-nightly-version)"

    cargo +"$nightly" fmt --package math --check
    cargo +"$nightly" fmt --package llm-openai-chat-completion --check
    cargo +"$nightly" fmt --package payments-stripe --check

# Run clippy on all native Nexus Tools
clippy: _check-cargo
    cargo +stable clippy --package math
    cargo +stable clippy --package llm-openai-chat-completion
    cargo +stable clippy --package payments-stripe

# Runs the `which` native Nexus Tool. Note that some Tools might need some
# environment variables to be set.
run which: _check-cargo
    cargo +stable run --package {{which}}
