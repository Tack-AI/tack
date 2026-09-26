#!/bin/sh
# Reference solution for docs-audit-permissions-deny-precedence: derive the
# verdict from the implementation, then write a schema-conformant report.
set -eu

verdict=mismatch
if awk '/fn matches_deny/,/^    }/' impl_permissions.rs | grep -q 'iter().any(' \
   && awk '/fn matches_allow/,/^    }/' impl_permissions.rs | grep -q 'iter().all(' \
   && grep -q 'denied by permissions.deny rule' impl_permissions.rs; then
    chain=$(awk '/pub fn chain_with_deny_rules/,/^}/' impl_permissions.rs)
    deny_line=$(printf '%s\n' "$chain" | grep -n 'DenyRulesHooks' | head -1 | cut -d: -f1 || true)
    inner_line=$(printf '%s\n' "$chain" | grep -n '^        inner,$' | head -1 | cut -d: -f1 || true)
    if [ -n "$deny_line" ] && [ -n "$inner_line" ] && [ "$deny_line" -lt "$inner_line" ]; then
        verdict=match
    fi
fi

if [ "$verdict" = match ]; then
    drifts='[]'
else
    drifts='["deny precedence or batch-matching semantics contradict doc_configuration.md"]'
fi

cat > audit-report.json <<EOF
{
  "claim": "permissions.deny always wins (TUI + headless + bypass); deny matches ANY batch entry, allow requires EVERY entry; allow-always persists to permissions.json allowAlways",
  "verdict": "$verdict",
  "documentation_evidence": "doc_configuration.md section '权限（permissions.*）': 'deny 永远优先，且在 headless 模式同样生效'",
  "implementation_evidence": "impl_permissions.rs chain_with_deny_rules (DenyRulesHooks chained before inner), DenyRulesHooks::before_tool_call (headless guard), matches_deny(any) vs matches_allow(all), persist_allow_always",
  "drifts": $drifts
}
EOF
