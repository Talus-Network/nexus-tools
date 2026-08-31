#!/usr/bin/env bash
#
# audit.sh — mechanical static + dynamic audit of a Nexus tool crate.
# Invoked by the nexus-tool-auditor agent; can also be run by hand.
#
# Usage:
#   audit.sh off-chain <crate-name>
#   audit.sh on-chain  <move-package-path>
#
# Off-chain crates live at offchain/tools/<crate>/. The workspace is at
# offchain/Cargo.toml.

set -uo pipefail

KIND="${1:?missing first arg: off-chain|on-chain}"
TARGET="${2:?missing second arg: crate name (off-chain) or path (on-chain)}"

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT"

WORKSPACE_MANIFEST="offchain/Cargo.toml"

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
  local crate_dir="offchain/tools/$crate"

  if [[ ! -d "$crate_dir" ]]; then
    emit FAIL "crate:exists" "$crate_dir not found"
    return
  fi

  # ---- C1 (CRITICAL): credential-shaped Input fields ---------------------
  # This is the central security check. Tool inputs flow through the Nexus
  # DAG on Sui as plaintext, so any credential-shaped field on Input is
  # effectively published on-chain. Refuse to mark the tool ready if any
  # match is found.
  #
  # Detection rule:
  #   - field name (case-insensitive) contains any of: api_key, apikey,
  #     secret, password, private_key, bearer, access_token, consumer_key,
  #     consumer_secret, client_secret, ends with _token, or is exactly
  #     `token` / `key`
  #   - EXCEPT for the curated whitelist of legitimately public per-call
  #     values: idempotency_key, pagination_token, page_token, cursor_token,
  #     next_token, continuation_token, refresh_cursor
  : > /tmp/audit-input-cred.log
  local cred_keywords='api_key|apikey|secret|password|private_key|bearer|access_token|consumer_key|consumer_secret|client_secret'
  local cred_suffix='_token$'
  local cred_exact='^(token|key)$'
  local cred_whitelist='^(idempotency_key|pagination_token|page_token|cursor_token|next_token|continuation_token|refresh_cursor)$'
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    # awk: find every line of every `struct Input` block (between the
    # struct opener and the matching closing brace at column 1) and check
    # field names. Field syntax: `pub <name>: ...` or `<name>: ...`.
    awk -v file="$f" \
        -v keywords="$cred_keywords" \
        -v suffix="$cred_suffix" \
        -v exact="$cred_exact" \
        -v whitelist="$cred_whitelist" '
      BEGIN { in_input=0; depth=0 }
      # Match the Input struct header. Note: deliberately NOT using
      # IGNORECASE — it breaks gawks regex on subsequent lines for unclear
      # reasons. Field-name matching uses tolower() instead.
      /^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?struct Input[[:space:]<{]/ {
        in_input=1; depth=0
      }
      in_input {
        # naive brace counter — works for single-Input-per-file crates,
        # which is the universal convention.
        for (i=1; i<=length($0); i++) {
          c=substr($0,i,1)
          if (c=="{") depth++
          else if (c=="}") { depth--; if (depth==0) { in_input=0; next } }
        }
        # Field detection: strip leading whitespace, optional `pub`, capture
        # the identifier before the colon.
        line=$0
        sub(/^[[:space:]]*/, "", line)
        sub(/^pub(\([^)]*\))?[[:space:]]+/, "", line)
        if (match(line, /^[a-zA-Z_][a-zA-Z0-9_]*[[:space:]]*:/)) {
          field=substr(line, RSTART, RLENGTH)
          sub(/[[:space:]]*:.*$/, "", field)
          field_lc = tolower(field)
          # Whitelist first.
          if (field_lc ~ whitelist) next
          # Then match against credential patterns.
          if (field_lc ~ keywords || field_lc ~ suffix || field_lc ~ exact) {
            print file ":" NR " Input." field
          }
        }
      }
    ' "$f" >> /tmp/audit-input-cred.log
  done < <(grep -rEl 'struct Input\b' "$crate_dir/src" --include='*.rs' 2>/dev/null || true)

  if [[ -s /tmp/audit-input-cred.log ]]; then
    emit FAIL "static:input-credential" "C1 violation — credential on Input flows to chain. See /tmp/audit-input-cred.log"
  else
    emit PASS "static:input-credential"
  fi

  # ---- Static: cargo ------------------------------------------------------
  if require cargo; then
    if cargo +stable check --manifest-path "$WORKSPACE_MANIFEST" --all-targets --package "$crate" 2>&1 | tee /tmp/audit-check.log | tail -5 >/dev/null; then
      if grep -q 'error\[' /tmp/audit-check.log; then
        emit FAIL "cargo:check" "see /tmp/audit-check.log"
      else
        emit PASS "cargo:check"
      fi
    else
      emit FAIL "cargo:check" "non-zero exit"
    fi

    if cargo +stable clippy --manifest-path "$WORKSPACE_MANIFEST" --all-targets --all-features --package "$crate" -- -D warnings 2>&1 | tee /tmp/audit-clippy.log | tail -5 >/dev/null; then
      if grep -q 'warning:\|error\[' /tmp/audit-clippy.log; then
        emit FAIL "cargo:clippy" "see /tmp/audit-clippy.log"
      else
        emit PASS "cargo:clippy"
      fi
    else
      emit FAIL "cargo:clippy" "non-zero exit"
    fi

    if cargo +stable test --manifest-path "$WORKSPACE_MANIFEST" --no-run --package "$crate" 2>&1 | tail -5 >/dev/null; then
      emit PASS "cargo:test-compiles"
    else
      emit FAIL "cargo:test-compiles"
    fi
  fi

  if require cargo-audit; then
    if (cd offchain && cargo audit) 2>&1 | tee /tmp/audit-audit.log | grep -q 'Crate:'; then
      emit FAIL "cargo:audit" "advisories present — see /tmp/audit-audit.log"
    else
      emit PASS "cargo:audit"
    fi
  fi

  if require cargo-deny; then
    if (cd offchain && cargo deny check) 2>&1 | tail -5 >/dev/null; then
      emit PASS "cargo:deny"
    else
      emit WARN "cargo:deny" "non-zero exit"
    fi
  fi

  # ---- Static: greps ------------------------------------------------------
  # 1. unwrap/expect/panic — flag any usage in non-test code. Caller can
  # rule out startup-only unwraps (Client::from_env in NexusTool::new)
  # when writing AUDIT.md, but every site needs eyeballing.
  : > /tmp/audit-unwrap.log
  while IFS= read -r f; do
    grep -nE '\b(unwrap|expect|panic!)\b' "$f" 2>/dev/null \
      | grep -v 'mod tests' \
      | awk -v file="$f" -F: '{print file":"$0}' \
      >> /tmp/audit-unwrap.log || true
  done < <(find "$crate_dir/src" -name '*.rs' -not -path '*/tests/*')
  if [[ -s /tmp/audit-unwrap.log ]]; then
    emit WARN "static:unwrap-in-non-test" "$(wc -l </tmp/audit-unwrap.log) site(s) — review each manually — see /tmp/audit-unwrap.log"
  else
    emit PASS "static:unwrap-in-non-test"
  fi

  # 2. println/dbg in non-test code (use log::* instead)
  grep -rn -E '\b(println!|eprintln!|dbg!)\b' "$crate_dir/src" --include='*.rs' \
    | grep -v 'mod tests' >/tmp/audit-println.log || true
  if [[ -s /tmp/audit-println.log ]]; then
    emit WARN "static:println" "$(wc -l </tmp/audit-println.log) site(s) — see /tmp/audit-println.log"
  else
    emit PASS "static:println"
  fi

  # 3. danger_accept_invalid_certs (C4)
  if grep -rn 'danger_accept_invalid_certs' "$crate_dir/src" --include='*.rs' >/dev/null; then
    emit FAIL "static:disabled-tls-verify" "C4 violation — danger_accept_invalid_certs present"
  else
    emit PASS "static:disabled-tls-verify"
  fi

  # 4. hardcoded secrets (C5 heuristic)
  if grep -rEn '(api[_-]?key|secret|token|bearer)\s*=\s*"[A-Za-z0-9_\-]{16,}"' "$crate_dir/src" --include='*.rs' >/tmp/audit-secrets.log; then
    if [[ -s /tmp/audit-secrets.log ]]; then
      emit WARN "static:hardcoded-secret" "see /tmp/audit-secrets.log"
    else
      emit PASS "static:hardcoded-secret"
    fi
  else
    emit PASS "static:hardcoded-secret"
  fi

  # 4b. Stripe-style live-key leak detector (C11). Require ≥16 alnum chars
  # after the prefix to avoid matching README explanatory text.
  if grep -rEn '(sk|pk|rk)_live_[A-Za-z0-9]{16,}' "$crate_dir" --include='*.rs' --include='*.md' --include='*.json' --include='*.yaml' --include='*.yml' >/tmp/audit-livekey.log; then
    if [[ -s /tmp/audit-livekey.log ]]; then
      emit FAIL "static:live-key-leak" "C11 violation — see /tmp/audit-livekey.log"
    else
      emit PASS "static:live-key-leak"
    fi
  else
    emit PASS "static:live-key-leak"
  fi

  # 4c. statefulness sniff (C9): mutable shared state across requests.
  # OnceLock is allowed (used by clients for shared HTTP pools); flag
  # OnceCell/lazy_static/Mutex/RwLock/RefCell/static-mut/AtomicU instead.
  if grep -rEn '\b(lazy_static!|once_cell|Mutex<|RwLock<|RefCell<|static mut|AtomicU)' "$crate_dir/src" --include='*.rs' \
       | grep -v 'mod tests' >/tmp/audit-stateful.log; then
    if [[ -s /tmp/audit-stateful.log ]]; then
      emit FAIL "static:stateful" "C9 violation: tool must be stateless. See /tmp/audit-stateful.log"
    else
      emit PASS "static:stateful"
    fi
  else
    emit PASS "static:stateful"
  fi

  # 4d. on-disk writes outside tests
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

  # 4e. Debug derived on a struct that contains a bearer / secret /
  # api_key / private_key / token field (C8 violation). The credential
  # type should hand-implement Debug to print <redacted>.
  if grep -rEn -B3 'pub bearer:|pub api_key:|pub .*_secret:|pub .*_token:|pub private_key:' "$crate_dir/src" --include='*.rs' \
       | grep -E '#\[derive\([^)]*Debug' >/tmp/audit-debug-cred.log; then
    if [[ -s /tmp/audit-debug-cred.log ]]; then
      emit FAIL "static:debug-on-credentials" "C8 violation: Debug derived near credential field. See /tmp/audit-debug-cred.log"
    else
      emit PASS "static:debug-on-credentials"
    fi
  else
    emit PASS "static:debug-on-credentials"
  fi

  # 4f. tools.json present + shape (H7) — needs jq for the shape check;
  # gracefully WARN if jq is not installed locally (CI has it).
  if [[ ! -f "$crate_dir/tools.json" ]]; then
    emit FAIL "tools-json:exists" "H7 violation: tools.json missing — shared discover workflow will skip this tool"
  elif require jq; then
    if jq -e '(.tool_name | type == "string") and (.command | type == "string") and ((.environment // {}) | type == "object")' "$crate_dir/tools.json" >/dev/null 2>&1; then
      # tool_name must equal command must equal Cargo.toml's [[bin]].name
      local tn cmd binname
      tn=$(jq -r '.tool_name' "$crate_dir/tools.json")
      cmd=$(jq -r '.command' "$crate_dir/tools.json")
      binname=$(grep -A2 '^\[\[bin\]\]' "$crate_dir/Cargo.toml" 2>/dev/null | grep '^name = ' | head -1 | sed -E 's/name = "([^"]+)"/\1/')
      if [[ -z "$binname" ]]; then
        emit WARN "tools-json:name-match" "tool_name=$tn command=$cmd; no [[bin]] section in Cargo.toml yet (skill caller adds it)"
      elif [[ "$tn" != "$cmd" ]] || [[ "$tn" != "$binname" ]]; then
        emit FAIL "tools-json:name-match" "tool_name=$tn command=$cmd [[bin]].name=$binname — must all match"
      else
        emit PASS "tools-json:shape"
      fi
    else
      emit FAIL "tools-json:shape" "tools.json missing tool_name/command/environment"
    fi

    # 4g. C10: tools.json environment block must NOT contain anything
    # secret-looking. The pipeline merges this into Cloud Run's env, which
    # is project-readable.
    if jq -e '
      [.environment // {} | to_entries[] | .key]
      | map(test("(api[_-]?key|secret|token|bearer|password|private[_-]?key|access[_-]?token)"; "i"))
      | any
    ' "$crate_dir/tools.json" >/dev/null 2>&1; then
      emit FAIL "tools-json:secret-in-env" "C10 violation: tools.json environment lists a secret-looking var — use Cloud Run secretKeyRef instead"
    else
      emit PASS "tools-json:secret-in-env"
    fi
  fi

  # 4h. build.rs present (required to thread TOOL_FQN_VERSION)
  if [[ ! -f "$crate_dir/build.rs" ]]; then
    emit WARN "build-rs:exists" "build.rs missing — TOOL_FQN_VERSION won't be threaded into FQNs"
  else
    emit PASS "build-rs:exists"
  fi

  # 4i. No legacy per-tool deploy plumbing left behind (post-#33 cleanup)
  for legacy in "$crate_dir/deploy" "$crate_dir/paths.json" \
                ".github/workflows/deploy-${crate}-testnet.yml" \
                ".github/workflows/deploy-${crate}-mainnet.yml"; do
    if [[ -e "$legacy" ]]; then
      emit FAIL "legacy:per-tool-plumbing" "$legacy must be deleted — superseded by shared offchain-tools.* pipeline"
    fi
  done

  # 5. Output is enum (C2)
  if grep -rn 'enum Output' "$crate_dir/src" --include='*.rs' >/dev/null; then
    emit PASS "conform:output-enum"
  else
    emit FAIL "conform:output-enum" "C2: no enum Output found — Nexus requires top-level oneOf"
  fi

  # 6. deny_unknown_fields on every Input (H1)
  local input_files
  input_files="$(grep -rEln 'struct Input\b' "$crate_dir/src" --include='*.rs' || true)"
  if [[ -n "$input_files" ]]; then
    while IFS= read -r f; do
      if ! grep -B2 'struct Input\b' "$f" | grep -q 'deny_unknown_fields'; then
        emit WARN "conform:deny-unknown-fields" "$f"
      fi
    done <<< "$input_files"
  fi

  # 7. description() override (M1)
  if grep -rn 'fn description' "$crate_dir/src" --include='*.rs' >/dev/null; then
    emit PASS "conform:description"
  else
    emit WARN "conform:description" "no description() override — /meta will show empty"
  fi

  # 8. FQN threaded through TOOL_FQN_VERSION (X1)
  if grep -rn 'fqn!(concat!(' "$crate_dir/src" --include='*.rs' >/dev/null \
     && grep -rn 'env!("TOOL_FQN_VERSION")' "$crate_dir/src" --include='*.rs' >/dev/null; then
    emit PASS "conform:fqn-versioned"
  else
    emit WARN "conform:fqn-versioned" "FQNs are not threaded through env!(\"TOOL_FQN_VERSION\") — version will stay at @1 forever"
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
