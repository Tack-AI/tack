# Herdr 集成

**[English](herdr.md) | 简体中文**

Tack 内置了 [Herdr](https://herdr.dev) 客户端。Herdr 是面向编码智能体的终端工作区管理器。在 Herdr pane 中运行 `tack`，它就会作为一等 `tack` 智能体出现——不需要 Herdr fork、插件、integration 安装器，也不需要修改 Tack 设置。

## 支持的能力

- **生命周期状态**：Herdr 会显示 pane 的 `working`、`idle` 或 `blocked` 状态。权限确认、`ask_user`、MCP elicitation 以及与 agent 运行相关的扩展对话框都视为 blocked。
- **自动化与通知**：上报的状态可用于 Herdr 侧边栏、通知、`herdr agent list` 和 `herdr agent wait`。
- **会话恢复**：对于持久化会话，Tack 会上报 session ID 以及 `tack --agent-dir <dir> --session <id> --continue --session-dir <dir> --provider <provider> --model <model>` 恢复命令（设置 thinking level 时还会携带 `--thinking`）。Herdr 服务重启后，可以在原 pane 和工作目录中重新打开该会话并回放历史。通过 `/new`、`/resume` 等命令切换会话后也会重新上报，provider、model 或 thinking level 变化时也会更新恢复命令。
- **退出清理**：TUI 退出时，Tack 会释放该 Herdr pane 的 agent 所有权。如果释放报告无法送达，Herdr 仍可通过 shell 提示符检测作为兜底。

只有交互式 TUI 会向 Herdr 上报。print、JSONL RPC、ACP、远程模式以及嵌套的 headless 进程，不会仅因为继承了 Herdr 环境变量就抢占 pane 状态。

## 使用方法

安装 Herdr 官方发布版（会话恢复命令需要 Herdr 0.9.2 或更新版本），然后在项目所在目录启动：

```sh
brew install herdr       # 也可以使用 https://herdr.dev 上的安装器
cd /path/to/project
herdr
```

在 pane 中运行 `tack`。如果希望 Herdr 在服务重启后恢复会话，必须能从 `PATH` 找到 `tack` 可执行文件。

可以在另一个 pane 中检查：

```sh
herdr agent list
herdr pane get <pane-id>
herdr agent wait <agent-or-pane>
```

使用独立的命名会话测试恢复，不影响日常 Herdr 会话：

```sh
herdr --session tack-test
# 在测试 pane 中启动 tack，然后从另一个终端执行：
herdr session stop tack-test
herdr --session tack-test
```

`--no-session` 会话仍会报告生命周期状态，但没有可供恢复的持久化会话。如果 session ID 或目录不能用 Herdr 的恢复命令格式表示（例如包含单引号或控制字符），Tack 仍会上报会话身份，但会省略无效的恢复命令。

## 启用与退出集成

以下环境变量由 Herdr 提供，不要手动配置：

| 变量 | 用途 |
|---|---|
| `HERDR_ENV=1` | 让交互式 TUI 启用上报 |
| `HERDR_PANE_ID` | 标识拥有当前 Tack 进程的 pane |
| `HERDR_BIN_PATH` | 用于跨平台 CLI 上报的 Herdr 可执行文件 |
| `HERDR_SOCKET_PATH` | 该 CLI 继承的 Herdr 服务地址 |

上报采用异步、尽力而为的方式：重复状态会合并，每个命令都有短超时，Herdr 失败不会阻塞或打断 agent 主循环。报告使用单调递增的序列号，旧报告不能覆盖新状态。

如果要在 Herdr 中运行 TUI 但不启用上报，可以只为该进程移除启用变量：

```sh
# Unix
env -u HERDR_ENV tack
```

```powershell
# Windows PowerShell
Remove-Item Env:HERDR_ENV
tack
```

Herdr 按官方方式正常升级即可。Herdr 内没有安装任何 Tack 专用代码，任一程序升级后都不需要重新安装或重新生成集成文件。

## 上报的数据

Tack 只上报 agent 名称、生命周期状态、通用的“等待用户输入”消息、持久化 session ID，以及恢复命令所需的 agent 目录、session 目录、provider、model 和 thinking level。提示词、模型回复、工具参数、凭据和文件内容都不会包含在 Herdr 报告中。
