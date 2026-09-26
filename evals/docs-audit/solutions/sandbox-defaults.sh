#!/bin/sh
# Reference solution for docs-audit-sandbox-defaults: perform the audit the
# way the task intends (derive the verdict from the implementation, then
# write a schema-conformant audit-report.json).
set -eu

verdict=mismatch
if awk '/fn parse_sandbox_key/,/^}/' impl_settings.rs | grep -q 'unwrap_or(true)' \
   && grep -q 'settings\.sandbox = false' impl_settings.rs \
   && grep -q 'parse_sandbox_key(managed)' impl_settings.rs; then
    verdict=match
fi

if [ "$verdict" = match ]; then
    drifts='[]'
else
    drifts='["sandbox default or layer-merge behavior contradicts doc_configuration.md"]'
fi

cat > audit-report.json <<EOF
{
  "claim": "sandbox defaults to on; features.sandbox overrides the legacy sandbox key; global may enable, project may only disable, managed forces either way",
  "verdict": "$verdict",
  "documentation_evidence": "doc_configuration.md sections '配置层级与优先级' and '沙箱': sandbox defaults to \"on\" and the project layer may only disable it",
  "implementation_evidence": "impl_settings.rs parse_sandbox_key (unwrap_or(true) default, features.sandbox precedence) and the Settings::load merge (managed wins, project can only set false)",
  "drifts": $drifts
}
EOF
