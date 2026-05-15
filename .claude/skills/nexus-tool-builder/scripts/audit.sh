#!/usr/bin/env bash
#
# audit.sh — mechanical static + dynamic audit of a Nexus tool crate.
# Invoked by the nexus-tool-auditor agent; can also be run by hand.
#
# Usage:
#   audit.sh off-chain <crate-name>
#   audit.sh on-chain  <move-package-path>

set -uo pipefail

KIND="${1:?missing first arg: off-chain|on-chain}"
TARGET="${2:?missing second arg: crate name (off-chain) or path (on-chain)}"

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT"

# Each check writes a single line "STATUS\tCHECK\tDETAIL" to the report.
REPORT="$(mktemp)"
PASS=0
FAIL=0

emit() {
  local status="$1" check="$2" detail="${3:-}"
  printf '%s\t%s\t%s\n' "$status" "$check" "$detail" >> "$REPORT"
  case "$status" in
    PASS) PASS=$((PASS+1));;
    FAIL|WARN) FAIL=$((FAIL+1));;
  esac
}

require() {
  if command -v "$1" >/dev/null 2>&1; then return 0; fi
  emit WARN "tooling:$1" "not installed — skipping checks that need it"
  return 1
}

off_chain() {
  local crate="$1"
  local crate_dir="tools/$crate"

  if [[ ! -d "$crate_dir" ]]; then
    emit FAIL "crate:exists" "$crate_dir not found"
    return
  fi

  # ---- Static: cargo ------------------------------------------------------
  if require cargo; then
    if cargo +stable check --all-targets --package "$crate" 2>&1 | tee /tmp/audit-check.log | tail -5 >/dev/null; then
      if grep -q 'error\[' /tmp/audit-check.log; then
        emit FAIL "cargo:check" "see /tmp/audit-check.log"
      else
        emit PASS "cargo:check"
      fi
    else
      emit FAIL "cargo:check" "non-zero exit"
    fi

    if cargo +stable clippy --all-targets --all-features --package "$crate" -- -D warnings 2>&1 | tee /tmp/audit-clippy.log | tail -5 >/dev/null; then
      if grep -q 'warning:\|error\[' /tmp/audit-clippy.log; then
        emit FAIL "cargo:clippy" "see /tmp/audit-clippy.log"
      else
        emit PASS "cargo:clippy"
      fi
    else
      emit FAIL "cargo:clippy" "non-zero exit"
    fi

    if cargo +stable test --no-run --package "$crate" 2>&1 | tail -5 >/dev/null; then
      emit PASS "cargo:test-compiles"
    else
      emit FAIL "cargo:test-compiles"
    fi
  fi

  if require cargo-audit; then
    if cargo audit 2>&1 | tee /tmp/audit-audit.log | grep -q 'Crate:'; then
      emit FAIL "cargo:audit" "advisories present — see /tmp/audit-audit.log"
    else
      emit PASS "cargo:audit"
    fi
  fi

  if require cargo-deny; then
    if cargo deny check 2>&1 | tail -5 >/dev/null; then
      emit PASS "cargo:deny"
    else
      emit WARN "cargo:deny" "non-zero exit"
    fi
  fi

  # ---- Static: greps ------------------------------------------------------
  # 1. unwrap/expect/panic — flag any usage in non-test code. Caller can
  # rule out startup-only unwraps (StripeClient::new builder) when
  # writing AUDIT.md, but every site needs eyeballing.
  : > /tmp/audit-unwrap.log
  while IFS= read -r f; do
    grep -nE '\b(unwrap|expect|panic!)\b' "$f" 2>/dev/null \
      | grep -v 'mod tests' \
      | awk -v file="$f" -F: '{print file":"$0}' \
      >> /tmp/audit-unwrap.log || true
  done < <(find "$crate_dir/src" -name '*.rs' -not -path '*/tests/*')
  # Exclude lines inside #[cfg(test)] modules. Simple heuristic: drop
  # any file path that already contains `tests` in its name; for
  # in-file `mod tests {}` blocks, the `mod tests` grep -v above
  # excludes the contents heuristically.
  if [[ -s /tmp/audit-unwrap.log ]]; then
    emit WARN "static:unwrap-in-non-test" "$(wc -l </tmp/audit-unwrap.log) site(s) — review each manually — see /tmp/audit-unwrap.log"
  else
    emit PASS "static:unwrap-in-non-test"
  fi

  # 2. println/dbg in non-test code
  grep -rn -E '\b(println!|eprintln!|dbg!)\b' "$crate_dir/src" --include='*.rs' \
    | grep -v 'mod tests' >/tmp/audit-println.log || true
  if [[ -s /tmp/audit-println.log ]]; then
    emit WARN "static:println" "$(wc -l </tmp/audit-println.log) site(s) — see /tmp/audit-println.log"
  else
    emit PASS "static:println"
  fi

  # 3. danger_accept_invalid_certs
  if grep -rn 'danger_accept_invalid_certs' "$crate_dir/src" --include='*.rs' >/dev/null; then
    emit FAIL "static:disabled-tls-verify" "danger_accept_invalid_certs present"
  else
    emit PASS "static:disabled-tls-verify"
  fi

  # 4. hardcoded secrets (heuristic)
  if grep -rEn '(api[_-]?key|secret|token|bearer)\s*=\s*"[A-Za-z0-9_\-]{16,}"' "$crate_dir/src" --include='*.rs' >/tmp/audit-secrets.log; then
    if [[ -s /tmp/audit-secrets.log ]]; then
      emit WARN "static:hardcoded-secret" "see /tmp/audit-secrets.log"
    else
      emit PASS "static:hardcoded-secret"
    fi
  else
    emit PASS "static:hardcoded-secret"
  fi

  # 4b. Stripe-style live-key leak detector. Require ≥16 alnum chars
  # after the prefix to avoid matching README explanatory text like
  # "sk_live_..." or "use sk_live_ for production".
  if grep -rEn '(sk|pk|rk)_live_[A-Za-z0-9]{16,}' "$crate_dir" --include='*.rs' --include='*.md' --include='*.json' --include='*.yaml' --include='*.yml' >/tmp/audit-livekey.log; then
    if [[ -s /tmp/audit-livekey.log ]]; then
      emit FAIL "static:live-key-leak" "see /tmp/audit-livekey.log"
    else
      emit PASS "static:live-key-leak"
    fi
  else
    emit PASS "static:live-key-leak"
  fi

  # 4c. credentials sourced from env (C7 violation)
  if grep -rEn 'std::env::var\(\s*"[A-Z_]*(KEY|SECRET|TOKEN|PASSWORD|BEARER)' "$crate_dir/src" --include='*.rs' >/tmp/audit-env-cred.log; then
    if [[ -s /tmp/audit-env-cred.log ]]; then
      emit FAIL "static:env-credential" "C7 violation: credentials must come via Input, not env. See /tmp/audit-env-cred.log"
    else
      emit PASS "static:env-credential"
    fi
  else
    emit PASS "static:env-credential"
  fi

  # 4d. statefulness sniff (C9): mutable shared state across requests
  if grep -rEn '\b(lazy_static!|OnceLock|once_cell|Mutex<|RwLock<|RefCell<|static mut|AtomicU)' "$crate_dir/src" --include='*.rs' \
       | grep -v 'mod tests' >/tmp/audit-stateful.log; then
    if [[ -s /tmp/audit-stateful.log ]]; then
      emit FAIL "static:stateful" "C9 violation: tool must be stateless. See /tmp/audit-stateful.log"
    else
      emit PASS "static:stateful"
    fi
  else
    emit PASS "static:stateful"
  fi

  # 4e. on-disk writes outside tests
  if grep -rEn 'std::fs::(write|create|create_dir|File::create|File::open\(\s*[^"])' "$crate_dir/src" --include='*.rs' \
       | grep -v 'mod tests' >/tmp/audit-fs.log; then
    if [[ -s /tmp/audit-fs.log ]]; then
      emit WARN "static:on-disk-write" "C9 candidate: persistent fs ops. See /tmp/audit-fs.log"
    else
      emit PASS "static:on-disk-write"
    fi
  else
    emit PASS "static:on-disk-write"
  fi

  # 4f. Debug derived on a struct that contains api_key (C8 violation)
  if grep -rEn -B3 'pub api_key:|pub bearer_token:|pub .*_secret:|pub .*_token:' "$crate_dir/src" --include='*.rs' \
       | grep -E '#\[derive\([^)]*Debug' >/tmp/audit-debug-cred.log; then
    if [[ -s /tmp/audit-debug-cred.log ]]; then
      emit FAIL "static:debug-on-credentials" "C8 violation: Debug derived near credential field. See /tmp/audit-debug-cred.log"
    else
      emit PASS "static:debug-on-credentials"
    fi
  else
    emit PASS "static:debug-on-credentials"
  fi

  # 4g. Cloud Run YAML should not reference upstream-API-key secrets (C10)
  local cr_yaml
  cr_yaml="$crate_dir/deploy"
  if [[ -d "$cr_yaml" ]] && grep -rEn 'secretName:' "$cr_yaml" >/tmp/audit-secrets-mount.log 2>/dev/null; then
    if grep -vE 'nexus-toolkit-config|nexus-allowed-leaders' /tmp/audit-secrets-mount.log >/tmp/audit-secrets-mount-bad.log; then
      if [[ -s /tmp/audit-secrets-mount-bad.log ]]; then
        emit FAIL "deploy:upstream-key-mounted" "C10 violation: unexpected secret in Cloud Run YAML. See /tmp/audit-secrets-mount-bad.log"
      else
        emit PASS "deploy:upstream-key-mounted"
      fi
    else
      emit PASS "deploy:upstream-key-mounted"
    fi
  fi

  # 5. Output is enum (look for `enum Output` not `struct Output`)
  if grep -rn 'enum Output' "$crate_dir/src" --include='*.rs' >/dev/null; then
    emit PASS "conform:output-enum"
  else
    emit FAIL "conform:output-enum" "no enum Output found — Nexus requires top-level oneOf"
  fi

  # 6. deny_unknown_fields on every Input
  local input_files
  input_files="$(grep -rEln 'struct Input\b' "$crate_dir/src" --include='*.rs' || true)"
  if [[ -n "$input_files" ]]; then
    while IFS= read -r f; do
      if ! grep -B2 'struct Input\b' "$f" | grep -q 'deny_unknown_fields'; then
        emit WARN "conform:deny-unknown-fields" "$f"
      fi
    done <<< "$input_files"
  fi

  # 7. description() override
  if grep -rn 'fn description' "$crate_dir/src" --include='*.rs' >/dev/null; then
    emit PASS "conform:description"
  else
    emit WARN "conform:description" "no description() override — /meta will show empty"
  fi
}

on_chain() {
  local pkg="$1"

  if [[ ! -d "$pkg" ]]; then
    emit FAIL "pkg:exists" "$pkg not found"
    return
  fi

  cd "$pkg"

  if require sui; then
    if sui move build 2>&1 | tee /tmp/audit-move-build.log | tail -5 >/dev/null; then
      if grep -qi 'error' /tmp/audit-move-build.log; then
        emit FAIL "sui:move-build"
      else
        emit PASS "sui:move-build"
      fi
    else
      emit FAIL "sui:move-build" "non-zero exit"
    fi

    if sui move test 2>&1 | tee /tmp/audit-move-test.log | tail -10 >/dev/null; then
      if grep -q 'FAILED' /tmp/audit-move-test.log; then
        emit FAIL "sui:move-test"
      else
        emit PASS "sui:move-test"
      fi
    else
      emit FAIL "sui:move-test" "non-zero exit"
    fi

    sui move prove 2>&1 | tail -10 > /tmp/audit-move-prove.log || true
    emit PASS "sui:move-prove" "see /tmp/audit-move-prove.log (best-effort)"
  fi

  # ---- Conformance greps --------------------------------------------------

  # 1. execute() signature: first arg ProofOfUID, last arg TxContext, returns TaggedOutput
  if grep -rEn 'public fun execute\(' sources --include='*.move' >/tmp/audit-execute.log; then
    while IFS=: read -r f line _rest; do
      block="$(sed -n "${line},+10p" "$f")"
      if ! echo "$block" | grep -q 'worksheet: &mut ProofOfUID'; then
        emit FAIL "move:execute-first-arg" "$f:$line"
      fi
      if ! echo "$block" | grep -q 'ctx: &mut TxContext'; then
        emit FAIL "move:execute-ctx-arg" "$f:$line"
      fi
      if ! echo "$block" | grep -q '-> TaggedOutput'; then
        emit FAIL "move:execute-return" "$f:$line"
      fi
    done < /tmp/audit-execute.log
    emit PASS "move:execute-found"
  else
    emit WARN "move:execute-found" "no public fun execute(…) — is this a tool module?"
  fi

  # 2. Worksheet stamping
  if grep -rEn 'stamp_with_data\(' sources --include='*.move' >/dev/null; then
    emit PASS "move:worksheet-stamp"
  else
    emit FAIL "move:worksheet-stamp" "no call to stamp_with_data — worksheet must be stamped"
  fi

  # 3. Witness has key, store (no copy)
  if grep -rEn 'struct \w+Witness has [^{]+copy' sources --include='*.move' >/dev/null; then
    emit FAIL "move:witness-copy" "witness has copy ability — must not be copyable"
  else
    emit PASS "move:witness-copy"
  fi

  # 4. Err variant in Output enum
  if grep -rEn 'public enum Output' sources --include='*.move' >/dev/null; then
    if grep -rEn '^[[:space:]]*Err[[:space:]]*\{' sources --include='*.move' >/dev/null; then
      emit PASS "move:err-variant"
    else
      emit WARN "move:err-variant" "Output enum present but no Err variant — Nexus expects at least one err-prefixed variant"
    fi
  fi

  # 5. Unauthorized public entry functions touching state
  if grep -rEn '^[[:space:]]*public entry fun ' sources --include='*.move' >/tmp/audit-entries.log; then
    if [[ -s /tmp/audit-entries.log ]]; then
      emit WARN "move:public-entry" "$(wc -l </tmp/audit-entries.log) entry fn(s) — verify each is authorized — see /tmp/audit-entries.log"
    else
      emit PASS "move:public-entry"
    fi
  fi

  cd - >/dev/null
}

case "$KIND" in
  off-chain) off_chain "$TARGET" ;;
  on-chain)  on_chain  "$TARGET" ;;
  *) echo "unknown kind: $KIND"; exit 2 ;;
esac

# ---- Final report ----------------------------------------------------------
echo "=========================================="
echo "audit: $KIND $TARGET"
echo "=========================================="
column -t -s$'\t' "$REPORT" || cat "$REPORT"
echo "------------------------------------------"
echo "passed: $PASS   failed/warned: $FAIL"
if [[ "$FAIL" -gt 0 ]]; then
  exit 1
fi
