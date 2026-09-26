# docs-audit — 文档与实现漂移审计

两层防线，追踪上游 TS pi commit `9211da172`（implementation-backed
documentation evals）的思路：**文档是被审计对象，代码是权威**。

## 1. 快速静态检查（无需模型）

```bash
./evals/docs-audit/static_check.sh    # exit 0 = 通过
```

grep 级交叉校验，四项：

| 检查 | 文档侧 | 代码侧 |
|---|---|---|
| settings.json 键 | `docs/configuration.md` 设置参考区的表格首列 | `crates/tack-app/src/settings.rs`（+ auth/project_trust/observability/shell_hooks/tui/mcp_config）实际读取的键 |
| hook 事件名 | `docs/hooks.md` 事件表 | `shell_hooks/config.rs` 的 `HookEvent::parse` |
| CLI flags | `README.md` + `docs/*.md` 里的 `--xxx` | `crates/tack-app/src/main.rs` 的 clap 定义（含 `#[command(version)]` 的 `--version`） |
| `features.*` 开关 | README/docs 里反引号的 `` `features.X` `` | settings.rs 的 `flag("X")` / `features.get("X")` 读取点 |

方向策略：

- **文档有而代码无 → FAIL**（真实漂移，脚本 exit 1）
- **代码有而文档无 → WARN**（允许未文档化的实验键；不改变退出码）

已知豁免写在脚本顶部的 `EXCLUDE_*` 清单里，每条都带原因注释。
**维护约定**：新增豁免必须注明原因；文档修复后应顺手删掉对应豁免，
让检查重新生效。WARN 出现新条目时，要么补文档，要么确认是有意不公开
并加进 `EXCLUDE_CODE_*`。

## 2. Agent 深度审计（需要模型）

```bash
export ANTHROPIC_API_KEY=...
tack eval evals/docs-audit                # 全部 3 个审计任务
tack eval evals/docs-audit --filter hooks # 按名字子串过滤
```

每个任务的 `setup` 把**当前**文档页和相关实现文件快照进任务目录，并用
确定性的 grep 锚点把"当前正确结论"算进 `.expected`；agent 深读实现后把
结论写成 `audit-report.json`（claim / verdict / documentation_evidence /
implementation_evidence / drifts）。`verify` 校验报告格式（mismatch 必须
附非空 drifts——发现的漂移必须写进报告）并要求 verdict 与实现事实一致。
因为期望值是 setup 时现算的，文档或代码任何一侧漂移都会让任务从
"agent 应报 match" 自动翻转成 "agent 应报 mismatch"，任务定义无需改动。

| 任务 | 审计的文档断言 |
|---|---|
| sandbox-defaults | 沙箱默认开启；`features.sandbox` 覆盖旧 `sandbox` 键；global 可开 / project 只能关 / managed 双向强制 |
| permissions-deny-precedence | deny 永远优先（TUI + headless + bypass）；deny 匹配批处理任一子命令、allow 要求全部匹配；always 答复持久化 `allowAlways` |
| hooks-verdict-protocol | 命令 hook 的 stdin/stdout verdict JSON 协议、exit 0/2/其他语义、多 handler 裁决合并（block 先到先得、deny > ask > allow）、fail-open、60s 默认超时 |

## 任务自检（无需模型）

```bash
./evals/docs-audit/selftest.sh          # 双向验证：fixture 未解必须 FAIL，
                                        # 参考解（solutions/）必须 PASS
```

与 `evals/selftest.sh` 同构；通过 `TACK_ROOT` 让 temp 目录里的 setup
找回仓库（`tack eval` 就地运行时默认 `../../..` 即可）。

参考解（`solutions/<task>.sh`）不读 `.expected` 作弊——它们自己重新执行
审计锚点、独立得出结论，用于验证任务定义自洽。

## 注意

- `tack eval` 在任务目录**就地**运行 setup/agent/verify（harness 行为，
  与 evals/examples 一致），产生的快照/报告文件已被 `.gitignore` 覆盖。
- setup 只依赖 POSIX shell + awk + grep + python3（verify），不需要网络。
