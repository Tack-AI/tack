# Connecting IntelliJ IDEA to Tack ACP

**English | [简体中文](intellij-idea-acp.zh-CN.md)**

Tack exposes its agent service over stdio via [ACP (Agent Client Protocol)](https://agentclientprotocol.com). JetBrains AI Assistant natively supports ACP, so IntelliJ IDEA (and the rest of the family — PyCharm, WebStorm, etc.) can connect to Tack directly.

Since Tack is not in JetBrains' ACP Registry, you need to configure it as a "custom agent" manually — two steps: **prepare the binary** + **register it in AI Assistant**.

## Prerequisites

- The **JetBrains AI Assistant** plugin installed and enabled (using an ACP agent does **not** require a JetBrains AI subscription).
- Known limitation: JetBrains' ACP support **does not work in WSL** environments.

## 1. Prepare the Tack executable

```bash
cd tack
cargo install --path crates/tack-app --locked
# Artifact: ~/.cargo/bin/tack (Windows: %USERPROFILE%\.cargo\bin\tack.exe)
```

You can also use a prebuilt package from GitHub releases.

After installing, verify in a terminal that ACP mode starts correctly:

```bash
tack acp   # runs JSON-RPC over stdio; Ctrl+C to exit
```

## 2. Register the agent in IDEA

1. Open the **AI Chat** tool window.
2. Click the settings button in the top-right corner of the tool window and choose **Add Custom Agent**. The IDE will create and open `~/.jetbrains/acp.json`.
3. Fill in the configuration:

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

Configuration notes:

- Use the **full path** for `command` (e.g. `C:/Users/<you>/.cargo/bin/tack.exe` on Windows) to avoid PATH resolution issues.
- Put the API key for your provider in `env` (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `CODEBUDDY_API_KEY`, etc., depending on the provider you use). If it's already configured in your system environment variables, `env` can be omitted.
- You can configure multiple agents at once — just add more entries side by side under `agent_servers`.

4. After saving, Tack immediately appears in the AI Chat agent selection list; select it to start chatting.

## Tack ACP capabilities

How the ACP interfaces Tack implements show up in IDEA:

- Streaming output: `agent_message_chunk` / `agent_thought_chunk`.
- Permission prompts: `session/request_permission`, with an allow-always cache.
- **Mode selector**: `ask` (default; edits/commands require confirmation), `acceptEdits` (file edits skip confirmation), `plan` (read-only), `bypass` (everything skips confirmation).
- **Model selector**: model list from the provider catalog.
- **Thinking-effort selector**: provided per the model's `thinkingLevelMap`.
- Session loading: `session/load` supports history replay.

## MCP integration (optional)

If you want Tack to use the MCP servers configured in IDEA (including the IntelliJ built-in MCP Server), add this to `acp.json`:

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

Tack will load these MCP configurations from the `mcpServers` passed in by the client.

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| Agent doesn't appear in the list | Check that `acp.json` is valid JSON; restart the IDE |
| Agent fails to start | Make sure `command` is a full path; run `tack acp` manually in a terminal to verify |
| Need detailed logs | AI Chat top-right → **Get ACP Logs**; for even more detailed request/response logs, enable `llm.agent.extended.logging` in the Registry (double-press `Shift` → type Registry) and restart the IDE |

> Note: with `llm.agent.extended.logging` enabled, logs may contain sensitive information such as conversation content — redact before sharing.

## References

- JetBrains official docs: [Agent Client Protocol (ACP)](https://www.jetbrains.com/help/ai-assistant/acp.html)
- For Zed setup, see the "Zed setup" section of `README.md` (the configuration format is essentially the same as `acp.json`).
