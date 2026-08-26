# Strict shape check for the runtime-authority-generation Toolkit config
# (nexus-sdk #562+; emitted by generate-signed-http-keys v2.0.0-rc8+).
# Differences from v2 (toolkit-config-v2.jq):
#   - no top-level `version` field (the toolkit no longer versions the file)
#   - per-tool `tool_signing_key` renamed to `response_signing_key`
#   - `replay_cache_ttl_ms` dropped from the generator output
(type == "object")
and ((keys - ["invoke_max_body_bytes", "signed_http"]) | length == 0)
and (.signed_http | type == "object")
and (.signed_http.mode == "required")
and ((.signed_http | keys - ["allowed_leaders", "allowed_leaders_path", "mode", "tools"]) | length == 0)
and ((.signed_http.allowed_leaders_path | type) == "string")
and ((.signed_http.allowed_leaders_path | length) > 0)
and (.signed_http.tools | type == "object")
and (($expected_fqns | length) > 0)
and ((.signed_http.tools | keys | sort) == $expected_fqns)
and ([
  .signed_http.tools[] |
  (type == "object")
  and ((keys - ["response_signing_key"]) | length == 0)
  and ((.response_signing_key | type) == "string")
  and ((.response_signing_key | length) > 0)
] | all)
