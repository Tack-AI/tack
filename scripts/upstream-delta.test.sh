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

# Expanded directory groups and the stable/experimental view use the same
# offline repository. Each path gets a real commit so --name-only grouping is
# exercised rather than mocked.
commit_path() {
    path=$1
    subject=$2
    mkdir -p "$upstream/$(dirname "$path")"
    printf '%s\n' "$subject" > "$upstream/$path"
    git -C "$upstream" add "$path"
    git -C "$upstream" commit -q -m "$subject"
}

commit_path packages/ai/index.ts 'fix(ai): stable parser (#101)'
commit_path packages/tui/index.ts 'fix(tui): redraw (#102)'
commit_path apps/p2/index.ts 'feat(p2): project menu (#103)'
commit_path packages/chord/index.ts 'feat(chord): delta sync (#104)'
commit_path apps/p3/index.ts 'feat(p3): host skeleton (#105)'
printf '%s\n' mixed > "$upstream/packages/ai/mixed.ts"
mkdir -p "$upstream/packages/durable"
printf '%s\n' mixed > "$upstream/packages/durable/index.ts"
git -C "$upstream" add packages/ai/mixed.ts packages/durable/index.ts
git -C "$upstream" commit -q -m 'feat: cross stable and durable (#106)'

work="$tmp/english"
UPSTREAM_REPO="$upstream" "$work/scripts/upstream-delta.sh" -o "$work/grouped.md" \
    > /dev/null 2> "$work/grouped-stderr.log"
grep -Fq 'commit 总数：6' "$work/grouped.md"
grep -Fq '### packages/ai（2 个 commit）' "$work/grouped.md"
grep -Fq '### packages/tui（1 个 commit）' "$work/grouped.md"
grep -Fq '### packages/chord（1 个 commit）' "$work/grouped.md"
grep -Fq '### apps/p2（1 个 commit）' "$work/grouped.md"
grep -Fq '### apps/p3（1 个 commit）' "$work/grouped.md"
grep -Fq '## 稳定主线 / 实验架构视图' "$work/grouped.md"
grep -Fq '### 稳定主线（2 个 commit）' "$work/grouped.md"
grep -Fq '### 实验架构（4 个 commit）' "$work/grouped.md"

printf '%s\n' 'upstream-delta baseline parser: English and Chinese documents passed'
printf '%s\n' 'upstream-delta grouping: expanded directories and architecture views passed'
