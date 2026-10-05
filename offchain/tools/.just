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
    cargo +stable build --workspace --release

# Check all native Nexus Tools
check: _check-cargo
    cargo +stable check --workspace

# Run all tests in all native Nexus Tools
test: _check-cargo
    cargo +stable test --workspace

# Run rustfmt on all native Nexus Tools
fmt-check: _check-cargo
    #!/usr/bin/env bash

    nightly="$(just helpers::get-nightly-version)"

    cargo +"$nightly" fmt --all --check

# Run clippy on all native Nexus Tools
clippy: _check-cargo
    cargo +stable clippy --workspace

# Runs the `which` native Nexus Tool. Note that some Tools might need some
# environment variables to be set.
run which: _check-cargo
    cargo +stable run --package {{which}}
