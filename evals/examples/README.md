# Tack eval suite

端到端 agent 行为回归评测集（`tack eval` harness 的任务目录）。每个子目录一个
`task.json`：

```json
{
  "name": "fix-typo",
  "setup": "printf 'teh' > a.txt",       // 可选，跑 agent 前在任务目录执行
  "prompt": "fix the typo in a.txt",     // 发给 agent 的完整任务
  "verify": "grep -q the a.txt",         // 退出码 0 = 通过
  "timeoutSecs": 300
}
```

任务互相独立、临时目录运行、不依赖网络。已有的任务覆盖：

| 任务 | 覆盖的 agent 能力 |
|---|---|
| create-file | 最小 write 路径 |
| fix-typo | 精确 edit（oldText 匹配） |
| edit-constant | 单点编辑不伤及相邻内容 |
| rename-function | 跨文件引用更新（编辑 + 搜索） |
| run-and-fix | 运行-观察-修复回路（bash + 迭代） |
| sort-lines / dedupe-lines | 内容重写 + 约束遵守（顺序/去重） |
| git-commit | git 工具（或 bash）+ 权限模式 |
| extract-todos | 只读提取 + 新文件创建 + 不动原文件 |
| json-fix | 语法纠错 + 语义保持（结构化验证） |
| multi-file-edit | 签名变更波及多文件（定义 + 两个调用点 + 运行验证） |
| test-fix | 跑已有测试套件定位 bug，修实现且不改测试 |
| regex-replace | 正则批量替换（日期格式转换），其余文本不动 |
| config-tune | 改 JSON 配置单个字段，其余字节级不变 |
| bash-pipeline | shell 管道聚合日志统计写文件（精确 diff 判分） |
| error-diagnose | 根据保存的 traceback 定位并修复跨模块 bug |
| html-css | 从零生成满足结构约束的 HTML+CSS |
| sql-query | sqlite 聚合查询，结果按格式写文件（精确 diff 判分） |
| markdown-rewrite | 按规则重写 markdown（标题升级 + URL 包装）且语义保持 |
| append-only | 严格追加写：已有内容字节级不变 + 新条目落尾 |

## 任务自检（不需要模型）

`evals/selftest.sh` 对每个带参考解（`evals/solutions/<name>.sh`）的任务做双向
验证：未解的 fixture 跑 `verify` 必须 **FAIL**（防止 verify 形同虚设），应用参考解后
必须 **PASS**（防止任务不可解 / verify 过严）。新增或修改任务后先跑：

```bash
evals/selftest.sh            # 全部
evals/selftest.sh            # 按名字子串过滤
```

## 用法

```bash
export ANTHROPIC_API_KEY=...        # 或其他 provider 的 key
tack eval evals/examples --runs 3 --report report.json
tack eval evals/examples --runs 3 --report report.json --baseline eval-baseline.json
```

## 基线（回归门禁）

对 TS pi 行为做 parity 回归的推荐流程：

1. 每次发布前在稳定环境跑 `tack eval evals/examples --runs 5 --report eval-baseline.json`，
   把报告提交进仓库。
2. CI（模板见 `examples/ci/tack-eval.yml`）每次跑 eval 并 `--baseline` 对比，
   任一任务通过率下降即失败。
3. 新增/修改任务后必须重新生成基线，并在 PR 里单独说明。

注意事项：

- 评分依赖真实模型，pass rate 天然有波动；`--runs 3` 以上再比较，单次波动无意义。
- verify 只用 POSIX shell + python3，不假设 jq/node 等存在。
- 新任务必须能在 60 秒内被合格 agent 完成；超时算失败。
