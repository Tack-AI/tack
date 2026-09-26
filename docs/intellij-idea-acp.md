# IntelliJ IDEA 接入 Tack ACP

Tack 通过 [ACP (Agent Client Protocol)](https://agentclientprotocol.com) 在 stdio 上对外提供 agent 服务。JetBrains AI Assistant 原生支持 ACP，因此 IntelliJ IDEA(及 PyCharm、WebStorm 等全家桶)可以直接连接 Tack。

由于 Tack 不在 JetBrains 的 ACP Registry 中，需走"自定义 agent"手动配置，共两步:**准备二进制** + **在 AI Assistant 中注册**。

## 前置条件

- 已安装并启用 **JetBrains AI Assistant** 插件(使用 ACP agent **不需要** JetBrains AI 订阅)。
- 已知限制:JetBrains 的 ACP 支持**不适用于 WSL** 环境。

## 1. 准备 Tack 可执行文件

```bash
cd tack
cargo install --path crates/tack-app --locked
# 产物:~/.cargo/bin/tack(Windows 为 %USERPROFILE%\.cargo\bin\tack.exe)
```

也可以直接使用 GitHub release 的预编译包。

安装后先在终端验证 ACP 模式能正常启动:

```bash
tack acp   # 在 stdio 上运行 JSON-RPC,Ctrl+C 退出
```

## 2. 在 IDEA 中注册 agent

1. 打开 **AI Chat** 工具窗口。
2. 点击工具窗口右上角的设置按钮,选择 **Add Custom Agent**。IDE 会创建并打开 `~/.jetbrains/acp.json`。
3. 填入配置:

```json
{
  "agent_servers": {
    "tack": {
      "command": "/home/<you>/.cargo/bin/tack",
      "args": ["acp"],
      "env": {
        "ANTHROPIC_API_KEY": "your-api-key-here"
      }
    }
  }
}
```

配置要点:

- `command` 建议使用**完整路径**(Windows 下如 `C:/Users/<you>/.cargo/bin/tack.exe`),避免 PATH 解析问题。
- `env` 放对应 provider 的 API key(`ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`CODEBUDDY_API_KEY` 等,取决于使用的 provider)。如果已在系统环境变量中配置,`env` 可省略。
- 可同时配置多个 agent,`agent_servers` 下并列添加即可。

4. 保存后,Tack 立即出现在 AI Chat 的 agent 选择列表中,选中即可对话。

## Tack ACP 能力说明

Tack 实现的 ACP 接口在 IDEA 中的表现:

- 流式输出:`agent_message_chunk` / `agent_thought_chunk`。
- 权限提示:`session/request_permission`,带 allow-always 缓存。
- **模式选择器**:`ask`(默认,编辑/命令需确认)、`acceptEdits`(文件编辑免确认)、`plan`(只读)、`bypass`(全部免确认)。
- **模型选择器**:provider catalog 中的模型列表。
- **思考强度选择器**:按模型的 `thinkingLevelMap` 提供。
- 会话加载:`session/load` 支持历史回放。

## MCP 集成(可选)

若希望 Tack 使用 IDEA 中配置的 MCP server(包括 IntelliJ 内置 MCP Server),在 `acp.json` 中增加:

```json
{
  "default_mcp_settings": {
    "use_idea_mcp": true,
    "use_custom_mcp": true
  },
  "agent_servers": {
    "tack": { "...": "..." }
  }
}
```

Tack 会从客户端传入的 `mcpServers` 中加载这些 MCP 配置。

## 排障

| 症状 | 处理 |
| --- | --- |
| agent 不出现在列表 | 检查 `acp.json` JSON 格式是否正确;重启 IDE |
| agent 启动失败 | 确认 `command` 为完整路径;在终端手动运行 `tack acp` 验证 |
| 需要查看详细日志 | AI Chat 右上角 → **Get ACP Logs**;更详细的请求/响应日志可在 Registry(`Shift` 双击 → 输入 Registry)中开启 `llm.agent.extended.logging` 后重启 IDE |

> 注意:开启 `llm.agent.extended.logging` 后,日志可能包含对话内容等敏感信息,分享前请脱敏。

## 参考

- JetBrains 官方文档:[Agent Client Protocol (ACP)](https://www.jetbrains.com/help/ai-assistant/acp.html)
- Zed 配置方式见 `README.md` 的 "Zed setup" 一节(配置格式与 `acp.json` 基本一致)。
