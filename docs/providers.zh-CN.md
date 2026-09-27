# Provider 深入指南

**[English](providers.md) | 简体中文**

面向用户与贡献者的 provider 细节：认证链、自定义 provider schema、以及各
provider 的适配器特性。registry 总览与模型目录见根目录
[README.md](../README.md)；逐 provider 的 env var 与设置键见
[configuration.md](configuration.zh-CN.md)。

## 认证解析顺序

每次 LLM 调用经 `AuthResolver` 解析凭据：

1. `--api-key` CLI 标志
2. `~/.tack/agent/models.json` 中该 provider 的 `apiKey`
3. provider 的环境变量（变量名与 TS pi 相同，如 `ZAI_API_KEY`、
   `MOONSHOT_API_KEY`、`KIMI_API_KEY`）
4. `auth.json` 中存储的凭据（`tack login` 写入）

## OAuth 登录流程

`tack login --provider <id>` 在未给 `--api-key` 时运行 provider 的 OAuth
流程（移植自 TS pi 的 `auth/oauth/*`）：

| Provider | 流程 | 说明 |
|---|---|---|
| **anthropic** | 浏览器 PKCE | Claude Pro/Max；loopback 端口 53692 |
| **openai-codex** | 浏览器（端口 1455）或 `--device-code` | ChatGPT Plus/Pro |
| **github-copilot** | device code | 企业账户会询问域名 |
| **openrouter** | PKCE | 换发永久 key |
| **kimi-coding** | device 流程 | |
| **xai** | device 流程 | |
| **radius** | device 流程 | |

浏览器流程用 loopback 回调与手动粘贴竞速；无头机器总是可以粘贴重定向
URL/code，或使用 `--device-code`。

凭据以 `{ "type": "oauth", access, refresh, expires, ... }` 存入
`auth.json`（与 TS pi 兼容）。过期 token 主动刷新：每进程双重检查锁，
获取锁后重读，使并发进程共享一次刷新，15 秒超时，刷新后持久化回去。
`tack auth-status` 显示每个 provider 的凭据类型和过期时间。

## 自定义 provider（models.json）

`~/.tack/agent/models.json`，schema 与 TS pi 相同：

```jsonc
{
  "providers": {
    "my-provider": {
      "baseUrl": "https://api.example.com/v1",
      "api": "openai-completions",          // 适配器类型
      "apiKey": "$MY_API_KEY",              // 支持 $VAR / ${VAR} 插值
      "headers": { "x-tenant": "…" },
      "compat": { /* 逐模型/逐 provider 怪癖覆盖 */ },
      "models": [ { "id": "…", "contextWindow": 128000, /* … */ } ]
    }
  }
}
```

`apiKey` 还支持 `!command` 形式：执行该命令并把 stdout 作为 key（与
TS pi 行为一致）。本地 provider 探测到的目录与 models.json 的
`contextWindow` 等字段做稀疏合并（models.json 优先）。

## Amazon Bedrock（`bedrock-converse-stream`）

不依赖 AWS SDK 实现：

- **SigV4 签名**（`bedrock/sigv4.rs`）——已用 AWS 文档的已知答案示例
  （known-answer test）验证；
- **AWS event-stream 二进制帧解码**（`bedrock/eventstream.rs`）——CRC 校验，
  split/coalesce 安全；
- **Converse 消息转换**（`bedrock/convert.rs`）。

**凭据链**（按优先级）：

1. `AWS_BEARER_TOKEN_BEDROCK` / `--api-key`（Bearer 模式，免 SigV4）
2. `AWS_PROFILE`（读 `~/.aws/credentials`）
3. 环境变量三元组 `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
   （含 `AWS_SESSION_TOKEN`）
4. `AWS_BEDROCK_SKIP_AUTH=1`（假凭据，本地调试）

**区域解析**：模型 id 中的 ARN → `AWS_REGION` / `AWS_DEFAULT_REGION` →
base-url 中的 region → `eu.` 前缀 → 默认 `us-east-1`。
SDK 默认链（IMDS/ECS/SSO/web-identity）**未实现**。

## Google Vertex（`google-vertex`）

两种认证模式：

- **Express API key**：`GOOGLE_CLOUD_API_KEY`；
- **ADC**（完整 google-auth-library 链）：
  1. `GOOGLE_APPLICATION_CREDENTIALS`（service-account JSON → RS256
     JWT-bearer 授权）；
  2. gcloud 通用 ADC 文件（`authorized_user` 刷新授权，gcloud 客户端常量）；
  3. GCE metadata server（`Metadata-Flavor: Google`，以亚秒超时探测，
     非 GCP 主机快速失败）。

token 缓存到临近过期。**project** 来自
`GOOGLE_CLOUD_PROJECT` / `GCLOUD_PROJECT` / SA `project_id` / metadata
server；**location** 来自 `GOOGLE_CLOUD_LOCATION`，缺省回退到 GCE 上由实例
zone 推导的 region（默认 `global`）。metadata server 的 base URL 可用
`TACK_GCE_METADATA_URL` 覆盖（测试用）。

## 本地 provider（零配置）

- **ollama**：默认 `http://localhost:11434/v1`，`OLLAMA_HOST` 覆盖；启动时
  探测 `/api/tags` 注入运行中服务器的模型。
- **llama.cpp**：默认 `http://localhost:8080/v1`，`LLAMA_CPP_HOST` 覆盖；
  探测 `/v1/models`。

探测约 500ms 超时、失败静默、`--offline` / `TACK_OFFLINE` 时跳过。未知上下文
窗口默认 32k，可用 models.json 按模型覆盖。无需登录或 API key。

## 模型目录

内嵌目录（构建时快照，1130 个模型）转换自已发布的
`@earendil-works/pi-ai` 包数据：上下文窗口、max tokens、推理标志、成本、
逐模型 `compat` 怪癖全部来自目录。`/models refresh` 可从 npm 拉取最新目录
（详见 [features.md](features.zh-CN.md) §模型目录在线刷新）。
