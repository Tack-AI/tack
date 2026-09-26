#!/bin/sh
# Reference solution for docs-audit-hooks-verdict-protocol: derive the
# verdict from the implementation, then write a schema-conformant report.
set -eu

verdict=mismatch
if awk '/fn parse_command_output/,/^}/' impl_engine.rs | grep -q '2 => HookOutput' \
   && grep -q '(Some(HookPermission::Deny), _) => false' impl_engine.rs \
   && grep -q 'Duration::from_secs(60)' impl_engine.rs \
   && grep -q 'hook {command} failed' impl_engine.rs; then
    verdict=match
fi

if [ "$verdict" = match ]; then
    drifts='[]'
else
    drifts='["hook verdict protocol (exit codes, verdict fields, merge rules, timeout) contradicts doc_hooks.md"]'
fi

cat > audit-report.json <<EOF
{
  "claim": "command-hook wire protocol: exit 0 verdict JSON schema, exit 2 = block with stderr reason, other non-zero = warning, first-block/most-restrictive-permission merging, fail-open, 60s default timeout",
  "verdict": "$verdict",
  "documentation_evidence": "doc_hooks.md section '命令 handler 协议' and the merge-semantics bullets below it",
  "implementation_evidence": "impl_engine.rs parse_command_output (exit-code arms), parse_verdict_json (field names), absorb_output (first block wins, deny > ask > allow), run_command (kill_on_drop, DEFAULT_TIMEOUT = 60s)",
  "drifts": $drifts
}
EOF
