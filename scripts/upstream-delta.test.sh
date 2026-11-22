#!/bin/sh
# Offline regression test for upstream-delta.sh's bilingual baseline parser.
set -eu

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
delta_script="$here/upstream-delta.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

upstream="$tmp/upstream"
git init -q "$upstream"
git -C "$upstream" config user.name 'Upstream Delta Test'
git -C "$upstream" config user.email 'upstream-delta@example.invalid'
git -C "$upstream" commit -q --allow-empty -m 'baseline'
base=$(git -C "$upstream" rev-parse HEAD)

for lang in english chinese; do
    work="$tmp/$lang"
    mkdir -p "$work/scripts" "$work/docs"
    cp "$delta_script" "$work/scripts/upstream-delta.sh"

    if [ "$lang" = english ]; then
        label='Aligned-to commit'
    else
        label='对齐到的 commit'
    fi
    cat > "$work/docs/upstream-alignment.md" <<EOF
# Test alignment document

| Item | Value |
| --- | --- |
| $label | \`$base\` (test baseline) |
EOF

    # There is no origin in this fixture: the script warns and uses local HEAD,
    # so the test never touches the network.
    UPSTREAM_REPO="$upstream" "$work/scripts/upstream-delta.sh" -o "$work/delta.md" \
        > /dev/null 2> "$work/stderr.log"
    grep -Fq "基线：\`$base\`" "$work/delta.md"
    grep -Fq 'commit 总数：0' "$work/delta.md"
    grep -Fq '无增量' "$work/delta.md"
done

printf '%s\n' 'upstream-delta baseline parser: English and Chinese documents passed'
