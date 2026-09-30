#!/bin/bash
# static_check.sh — grep-level documentation drift cross-checks.
# No model, no network. Mirrors the style of evals/selftest.sh.
#
# Checks (docs are the subject; code is authoritative):
#   1. settings.json keys documented in docs/configuration.md
#      vs keys actually read in crates/tack-app (settings.rs + friends)
#   2. hook event names documented in docs/hooks.md
#      vs HookEvent::parse() in crates/tack-app/src/shell_hooks/config.rs
#   3. CLI --flags documented in README.md / docs/*.md
#      vs clap definitions in crates/tack-app/src/main.rs
#   4. features.* keys documented vs features gates read in settings.rs
#
# Direction policy:
#   documented-but-not-implemented  => FAIL (real drift, script exits 1)
#   implemented-but-not-documented  => WARN (allowed for experimental keys;
#                                        silence known ones in the EXCLUDE
#                                        lists below, with a reason)
#
# Usage: evals/docs-audit/static_check.sh   (run from anywhere; exits 0 = pass)
set -euo pipefail

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
root=$(CDPATH= cd -- "$here/../.." && pwd)
cd "$root"

export LC_ALL=C

SETTINGS_RS=crates/tack-app/src/settings.rs
HOOKS_CONFIG_RS=crates/tack-app/src/shell_hooks/config.rs
MAIN_RS=crates/tack-app/src/main.rs
CONFIG_MD=docs/configuration.md
HOOKS_MD=docs/hooks.md

# ---------------------------------------------------------------------------
# Exclusion lists (documented-but-not-implemented side). Each entry needs a
# reason. Keep sorted. These are the ONLY drift items the script tolerates.
# ---------------------------------------------------------------------------

# Doc flags that are not tack CLI flags:
#   --all-targets --all --locked --workspace --package --release
#                                                            : cargo flags (release/dev docs;
#                                                              `cargo fmt --all` in audit-fix-plan-2026-09.md)
#   --oneline --get --hard --depth --path --git-dir        : git flags in examples
#   --generate-notes                                       : gh release flag (docs/release.md)
#   --stdio                                                : language-server flags (LSP docs)
#   --input-format --output-format --allowed               : codebuddy CLI flags (features.md)
#   --extension --no-extensions --plan                     : README §"与 TS pi 的差异" — documented
#                                                            precisely as NOT available in tack
#   --rpc                                                  : onboarding.md 架构图里 "--mode rpc" 的边标签
#   --no-install-recommends --pids-limit --cpus --memory --doc
#                                                            : docs/ci-ubuntu-wedge.md 引用的是
#                                                            apt/docker/cargo 命令的 flag，非 tack CLI
#   --manifest-path --quiet                                  : plugin-development.md 引用的是脚手架
#                                                            生成的 cargo 命令的 flag，非 tack CLI
#   --example                                              : codebuddy-pitfalls.md 的验证命令
#                                                            `cargo run -p tack-ai --example cb_rebuild_repro`
#                                                            引用的是 cargo flag，非 tack CLI
# Doc flags that are not tack's own CLI flags: cargo/rustup flags used in
# build instructions, and CodeBuddy CLI flags (`codebuddy` is the external
# tool tack embeds — docs/codebuddy.md and features.md describe ITS flags).
EXCLUDE_DOC_FLAGS="--all --all-targets --allowed --depth --example --extension --generate-notes --get --git-dir --hard --input-format --locked --no-extensions --oneline --output-format --package --path --plan --release --rpc --stdio --workspace --effort --include-partial-messages --permission-mode --setting-sources --strict-mcp-config --system-prompt --tools --no-install-recommends --pids-limit --cpus --memory --doc --manifest-path --quiet"

# Implemented-but-undocumented settings keys we deliberately do not warn about:
# (none currently — shellPath/appendSystemPrompt are documented in configuration.md)
EXCLUDE_CODE_SETTINGS_KEYS=""

# Implemented-but-undocumented CLI flags we do not warn about:
# (none currently — --allow-no-auth is documented in configuration.md)
EXCLUDE_CODE_FLAGS=""

# Doc settings keys that are parsed outside the scanned accessor patterns:
# (none currently — oauth and terminal.* are covered by the extra scan below)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

tmp=$(mktemp -d "${TMPDIR:-/tmp}/docs-audit.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

failures=0
warnings=0

in_list() { # in_list <word> <space-separated list>
    case " $2 " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

report_fail() { echo "FAIL  $1"; failures=$((failures + 1)); }
report_warn() { echo "WARN  $1"; warnings=$((warnings + 1)); }
report_ok()   { echo "ok    $1"; }

# ---------------------------------------------------------------------------
# Build the code-side settings key set.
# settings.rs reads keys via raw.get("k") / get_str("k") / get_bool(&raw,"k",d)
# / get_str_list / flag("k") / bool_key("k"); a few keys are read in other
# modules (auth.rs, project_trust.rs, observability.rs, shell_hooks/mod.rs,
# tui/mod.rs). mcp_config.rs parses mcp.json entries via serde fields.
# ---------------------------------------------------------------------------
SCAN_FILES="$SETTINGS_RS
crates/tack-app/src/auth.rs
crates/tack-app/src/project_trust.rs
crates/tack-app/src/observability.rs
crates/tack-app/src/plugin_policy.rs
crates/tack-app/src/marketplace_sync.rs
crates/tack-app/src/shell_hooks/mod.rs
crates/tack-app/src/tui/mod.rs
crates/tack-app/src/mcp_config.rs"

{
    # .get("camelCase") accessors
    grep -hoE '\.get\("[A-Za-z][A-Za-z0-9]*"\)' $SCAN_FILES
    # get_str("k") / get_bool(&raw, "k", d) / get_str_list(&raw, "k")
    grep -hE 'get_(str|bool|str_list)\(' $SCAN_FILES
    # flag("k") feature gates and bool_key("k") terminal overrides
    grep -hoE '(flag|bool_key)\("[A-Za-z][A-Za-z0-9]*"' $SETTINGS_RS
    # serde struct fields in mcp_config.rs (mcp.json entry keys)
    grep -hoE '^    [a-z_]+: ' crates/tack-app/src/mcp_config.rs
} | grep -oE '"[A-Za-z][A-Za-z0-9]*"|[a-z_]+: $' | tr -d '": ' \
  | while IFS= read -r w; do
      # snake_case serde field names → camelCase (mcp.json keys are camelCase)
      case "$w" in
        *_*) printf '%s\n' "$w" | awk -F_ '{out=$1; for(i=2;i<=NF;i++) out=out toupper(substr($i,1,1)) substr($i,2); print out}' ;;
        *)   printf '%s\n' "$w" ;;
      esac
    done | sort -u > "$tmp/code_settings_keys"

# Code-side TOP-LEVEL settings keys (for the WARN direction): the field
# assignments inside Settings::from_raw plus extras read in other modules.
{
    grep -hE '^            [a-z_]+: (get_str|get_bool|get_str_list|parse_|raw\.get)' "$SETTINGS_RS" \
        | grep -oE '"[A-Za-z][A-Za-z0-9]*"' | tr -d '"'
    printf '%s\n' credentialStore defaultProjectTrust auditSink managedHooksOnly imageProtocol extensionLockRequired
} | sort -u > "$tmp/code_top_keys"

# ---------------------------------------------------------------------------
# Check 1: settings.json keys
# ---------------------------------------------------------------------------
echo "== check 1: settings.json keys (docs/configuration.md vs crates/tack-app)"

# Documented keys: backticked first cells of the tables inside the
# "## settings.json full key reference" section, plus keys documented inline in prose.
sed -n '/^## settings.json full key reference/,/^## Environment variables/p' "$CONFIG_MD" \
    | grep -oE '^\| `[A-Za-z][A-Za-z0-9.]*`' | sed 's/^| `//; s/`$//' > "$tmp/doc_keys"
printf '%s\n' permissions.allow permissions.deny hooks managedHooksOnly mcpDeferThreshold >> "$tmp/doc_keys"
sort -u "$tmp/doc_keys" > "$tmp/doc_keys.sorted" && mv "$tmp/doc_keys.sorted" "$tmp/doc_keys"

# documented → must exist in code (both the top segment and, for dotted
# keys, the leaf segment must be read somewhere in the scanned code)
while IFS= read -r key; do
    top=${key%%.*}
    leaf=""
    case "$key" in *.*) leaf=${key#*.} ;; esac
    missing=""
    grep -qx "$top" "$tmp/code_settings_keys" || missing=$top
    if [ -n "$leaf" ] && ! grep -qx "$leaf" "$tmp/code_settings_keys"; then
        missing="$leaf (of $key)"
    fi
    if [ -n "$missing" ]; then
        report_fail "settings key \`$key\` documented in $CONFIG_MD but never read in code (missing: $missing)"
    fi
done < "$tmp/doc_keys"

# implemented top-level → should be documented (WARN only)
while IFS= read -r key; do
    in_list "$key" "$EXCLUDE_CODE_SETTINGS_KEYS" && continue
    grep -qF "\`$key" "$CONFIG_MD" \
        || report_warn "settings key \`$key\` read in code but not documented in $CONFIG_MD"
done < "$tmp/code_top_keys"

# ---------------------------------------------------------------------------
# Check 2: hook event names
# ---------------------------------------------------------------------------
echo "== check 2: hook events (docs/hooks.md vs HookEvent::parse)"

{ grep -oE '`[A-Z][A-Za-z]+`' "$HOOKS_MD" | tr -d '`'
  grep -oE '"[A-Z][A-Za-z]+"' "$HOOKS_MD" | tr -d '"'
} | sort -u > "$tmp/doc_events"

grep -oE '"[A-Z][A-Za-z]+" => HookEvent::' "$HOOKS_CONFIG_RS" \
    | grep -oE '"[A-Z][A-Za-z]+"' | tr -d '"' | sort -u > "$tmp/code_events"

while IFS= read -r ev; do
    grep -qx "$ev" "$tmp/code_events" \
        || report_fail "hook event \`$ev\` documented in $HOOKS_MD but not parsed by HookEvent::parse"
done < "$tmp/doc_events"
while IFS= read -r ev; do
    grep -qx "$ev" "$tmp/doc_events" \
        || report_warn "hook event \`$ev\` parsed in code but not documented in $HOOKS_MD"
done < "$tmp/code_events"

# ---------------------------------------------------------------------------
# Check 3: CLI flags
# ---------------------------------------------------------------------------
echo "== check 3: CLI flags (README.md + docs/*.md vs clap in main.rs)"

grep -ohE -- '--[a-z][a-z0-9-]+' README.md docs/*.md | sort -u > "$tmp/doc_flags"

# clap derive: `long = "name"` is explicit; bare `#[arg(long)]` kebab-cases
# the field name. `version` is generated by #[command(version)].
awk '
/#\[arg\(/ {
  attr = $0
  while (attr !~ /\]/ && (getline line) > 0) attr = attr " " line
  if (attr !~ /long/) next
  if (match(attr, /long = "[^"]+"/)) {
    print substr(attr, RSTART + 8, RLENGTH - 9)
    next
  }
  while ((getline line) > 0) {
    if (line ~ /^[[:space:]]*#/) continue
    if (match(line, /^[[:space:]]*[a-z_]+:/)) {
      name = substr(line, RSTART, RLENGTH - 1)
      gsub(/[[:space:]]/, "", name)
      gsub(/_/, "-", name)
      print name
      break
    }
  }
}' "$MAIN_RS" | sort -u > "$tmp/code_flags"
printf '%s\n' version >> "$tmp/code_flags"
sort -u "$tmp/code_flags" > "$tmp/code_flags.sorted" && mv "$tmp/code_flags.sorted" "$tmp/code_flags"

while IFS= read -r flag; do
    in_list "$flag" "$EXCLUDE_DOC_FLAGS" && continue
    grep -qx -- "${flag#--}" "$tmp/code_flags" \
        || report_fail "flag \`$flag\` documented but not parsed by the CLI (clap in $MAIN_RS)"
done < "$tmp/doc_flags"
while IFS= read -r flag; do
    in_list "--$flag" "$EXCLUDE_CODE_FLAGS" && continue
    grep -qx -- "--$flag" "$tmp/doc_flags" \
        || report_warn "flag \`--$flag\` parsed by the CLI but not documented in README.md/docs"
done < "$tmp/code_flags"

# ---------------------------------------------------------------------------
# Check 4: features.* gates
# ---------------------------------------------------------------------------
echo "== check 4: features.* keys (docs vs feature gates in settings.rs)"

# documented: backticked `features.X` mentions anywhere in README/docs
grep -ohE '`features\.[A-Za-z]+`' README.md docs/*.md \
    | sed 's/.*`features\.//; s/`//' | sort -u > "$tmp/doc_features"

# implemented: flag("X") reads + f.get("X") under the features object
# (parse_sandbox_key / managed enforcement)
{ grep -oE 'flag\("[A-Za-z]+"' "$SETTINGS_RS"
  grep -oE 'f\.get\("[A-Za-z]+"\)' "$SETTINGS_RS"
} | grep -oE '"[A-Za-z]+"' | tr -d '"' | sort -u > "$tmp/code_features"

while IFS= read -r feat; do
    grep -qx "$feat" "$tmp/code_features" \
        || report_fail "feature gate \`features.$feat\` documented but never read in code"
done < "$tmp/doc_features"
while IFS= read -r feat; do
    grep -qx "$feat" "$tmp/doc_features" \
        || report_warn "feature gate \`features.$feat\` read in code but not documented"
done < "$tmp/code_features"

# ---------------------------------------------------------------------------
echo "---"
echo "checks complete: $failures failure(s), $warnings warning(s)"
[ "$failures" -eq 0 ]
