#!/bin/sh
# 生成「上游自基线以来的增量审阅报告」（markdown，输出到 stdout 或 -o 指定文件）。
#
# 基线 commit 从 docs/upstream-alignment.md「当前基线」表的「对齐到的 commit」
# 一行解析（取该行第一个 40 位 hex），也可用环境变量 UPSTREAM_BASE 覆盖。
# 上游仓库由 UPSTREAM_REPO 指定：
#   - 本地路径：直接使用（先尝试 git fetch origin，失败则警告并用现状继续）；
#   - git URL（默认 https://github.com/earendil-works/pi.git）：
#     git clone --filter=blob:none 到临时目录，脚本退出时自动清理。
#
# 用法：
#   scripts/upstream-delta.sh                          # 报告输出到 stdout
#   scripts/upstream-delta.sh -o delta.md              # 写入文件
#   UPSTREAM_REPO=/path/to/pi scripts/upstream-delta.sh
#   UPSTREAM_BASE=<40位hash> scripts/upstream-delta.sh
#
# 每周由 .github/workflows/upstream-delta.yml 自动运行（结果发到标题含
# [upstream-delta] 的 issue）；手工审阅流程见 docs/upstream-alignment.md。
set -eu

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
doc="$here/../docs/upstream-alignment.md"
repo=${UPSTREAM_REPO:-https://github.com/earendil-works/pi.git}

outfile=
while getopts "o:h" opt; do
    case "$opt" in
        o) outfile=$OPTARG ;;
        h)
            echo "用法: $0 [-o 输出文件]"
            echo "环境变量: UPSTREAM_REPO（本地路径或 git URL）、UPSTREAM_BASE（覆盖基线 commit）"
            exit 0
            ;;
        *) echo "用法: $0 [-o 输出文件]" >&2; exit 2 ;;
    esac
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

# --- 1. 解析基线 commit ---------------------------------------------------
base=${UPSTREAM_BASE:-}
if [ -z "$base" ]; then
    if [ ! -f "$doc" ]; then
        echo "错误：找不到 $doc（可用 UPSTREAM_BASE 直接指定基线）" >&2
        exit 1
    fi
    # 「对齐到的 commit」一行形如：
    #   | 对齐到的 commit | `4a6ed01945c7f6a2a350996fb439148149ab65ee`（...） |
    # 提取该行第一个 40 位 hex。
    base=$(grep '对齐到的 commit' "$doc" | head -n 1 \
        | sed -n 's/.*`\([0-9a-f]\{40\}\)`.*/\1/p')
fi
if ! printf '%s' "$base" | grep -qE '^[0-9a-f]{40}$'; then
    echo "错误：无法解析基线 commit（得到 '$base'）。" >&2
    echo "  请检查 $doc 的「对齐到的 commit」一行，或用 UPSTREAM_BASE=<40位hash> 覆盖。" >&2
    exit 1
fi

# --- 2. 准备上游仓库 ------------------------------------------------------
if [ -d "$repo" ]; then
    if ! git -C "$repo" rev-parse --git-dir >/dev/null 2>&1; then
        echo "错误：$repo 存在但不是 git 仓库" >&2
        exit 1
    fi
    work=$repo
    if ! git -C "$work" fetch origin --quiet 2>/dev/null; then
        echo "警告：git -C $work fetch origin 失败（网络不可达或无 origin），使用本地现状继续" >&2
    fi
else
    case "$repo" in
        *://* | *@*:*) ;; # http(s)/ssh URL
        *)
            echo "错误：UPSTREAM_REPO='$repo' 既不是本地目录也不像 git URL" >&2
            exit 1
            ;;
    esac
    work="$tmp/upstream"
    echo "克隆 $repo ..." >&2
    if ! git clone --filter=blob:none --quiet "$repo" "$work" 2>"$tmp/clone.log"; then
        echo "错误：克隆上游仓库失败：$repo" >&2
        sed 's/^/  | /' "$tmp/clone.log" >&2
        echo "  网络诊断建议：检查网络/代理设置；可手动验证：git ls-remote $repo" >&2
        exit 1
    fi
fi

# --- 3. 校验基线并确定 HEAD -----------------------------------------------
if ! git -C "$work" cat-file -e "$base^{commit}" 2>/dev/null; then
    echo "错误：基线 commit $base 在上游仓库中不存在。" >&2
    echo "  可能上游历史被 rebase/force-push 改写，或 UPSTREAM_REPO 指向了错误的仓库。" >&2
    echo "  请核对 $doc 中的基线，必要时用 UPSTREAM_BASE 指定一个仍存在的 commit。" >&2
    exit 1
fi

# 本地路径场景下 HEAD 可能停在旧分支上，优先用 origin/HEAD。
if git -C "$work" rev-parse --verify --quiet refs/remotes/origin/HEAD >/dev/null 2>&1; then
    headref=origin/HEAD
else
    headref=HEAD
fi
head=$(git -C "$work" rev-parse "$headref")
total=$(git -C "$work" rev-list --count "$base..$head")
now=$(date -u '+%Y-%m-%d %H:%M UTC')

# --- 4. 按顶层目录粗分组 ---------------------------------------------------
# 单次 git log --name-only 取出每个 commit 触及的文件，再归并到
# packages/ai、packages/agent、packages/coding-agent、docs、其他 五组
# （一个 commit 触及多组时按上面优先级归入第一组，行内仍列出全部触及目录）。
mkdir -p "$tmp/groups"
group_file() {
    printf '%s/%s' "$tmp/groups" "$(printf '%s' "$1" | tr '/ ' '__')"
}

short=
subj=
paths=
flush_commit() {
    [ -n "$short" ] || return 0
    if [ -n "$paths" ]; then
        dirlist=$(printf '%s\n' "$paths" | awk -F/ '
            $1 == "packages" && NF >= 2 { print $1 "/" $2; next }
            NF >= 2 { print $1; next }
            { print "(根目录)" }' | sort -u)
    else
        dirlist="(无文件变更/合并提交)"
    fi
    group="其他"
    for want in packages/ai packages/agent packages/coding-agent docs; do
        if printf '%s\n' "$dirlist" | grep -qx "$want"; then
            group=$want
            break
        fi
    done
    ndirs=$(printf '%s\n' "$dirlist" | wc -l | tr -d ' ')
    if [ "$ndirs" -gt 3 ]; then
        summary="$(printf '%s\n' "$dirlist" | head -n 3 | tr '\n' ',' | sed 's/,$//; s/,/, /g')，等 $ndirs 处"
    else
        summary=$(printf '%s\n' "$dirlist" | tr '\n' ',' | sed 's/,$//; s/,/, /g')
    fi
    # 主题里若有 PR 号（形如 "(#1234)"）一并列出；主题已以其结尾时不重复追加
    pr=$(printf '%s' "$subj" | sed -n 's/.*(\(#[0-9][0-9]*\)).*/\1/p')
    if [ -n "$pr" ]; then
        case "$subj" in
            *"($pr)") pr= ;;
            *) pr="（${pr}）" ;;
        esac
    fi
    printf -- '- `%s` %s%s — 触及：%s\n' "$short" "$subj" "$pr" "$summary" \
        >> "$(group_file "$group")"
    short=
    subj=
    paths=
}

git -C "$work" log --reverse --no-renames --name-only \
    --format='COMMIT%x09%h%x09%s' "$base..$head" > "$tmp/raw.log"
while IFS= read -r line; do
    case "$line" in
        COMMIT"	"*)
            flush_commit
            rest=${line#COMMIT	}
            short=${rest%%	*}
            subj=${rest#*	}
            ;;
        "") ;; # commit 与文件列表之间的空行
        *) paths=${paths:+$paths
}$line ;;
    esac
done < "$tmp/raw.log"
flush_commit

# --- 5. 渲染报告 -----------------------------------------------------------
render() {
    echo "# 上游增量审阅报告"
    echo
    echo "- 生成时间：$now"
    echo "- 上游仓库：$repo"
    echo "- 基线：\`$base\`"
    echo "- 上游 HEAD：\`$head\`（${headref}）"
    echo "- commit 总数：$total"
    echo
    if [ "$total" -eq 0 ]; then
        echo "**无增量**：上游 HEAD 与基线一致（或基线已包含 HEAD），本轮无需审阅。"
        return 0
    fi
    echo "## 按上游目录分组"
    echo
    for g in packages/ai packages/agent packages/coding-agent docs "其他"; do
        f=$(group_file "$g")
        if [ -s "$f" ]; then
            n=$(wc -l < "$f" | tr -d ' ')
            echo "### ${g}（$n 个 commit）"
            echo
            cat "$f"
            echo
        fi
    done
    echo "## 全部 commit 清单"
    echo
    echo '```'
    git -C "$work" log --oneline "$base..$head"
    echo '```'
    echo
    echo "---"
    echo
    echo "审阅提示：按 docs/upstream-alignment.md 的「同步流程」逐条评估以上 commit"
    echo "（可直接落地 / 借鉴设计 / 无需跟进并记录原因），完成后把文档基线更新为"
    echo "\`$head\`。"
}

if [ -n "$outfile" ]; then
    render > "$outfile"
    echo "报告已写入 ${outfile}（基线 $base → HEAD ${head}，$total 个 commit）" >&2
else
    render
fi
