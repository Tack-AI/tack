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
`auth.json`（与 TS pi 兼容）。过期 token 主动刷新：每个 provider 的
**跨进程锁**覆盖「重读 → 刷新 → 持久化」完整周期，独立的 store 级锁避免
无关凭据写入互相丢失。刷新请求以 15 秒为限，开始后就与调用方分离，调用方
取消也不会丢掉轮换后的 refresh token；正常退出会等待已启动的刷新。提交前
再次比较当前凭据，绝不覆盖更新的登录结果，也不会在注销后复活旧授权。
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

`openai-completions` 模型支持以下兼容覆盖项：

| `compat` 字段 | 默认值 | 行为 |
| --- | --- | --- |
| `supportsDeveloperRole` | 仅 OpenAI 官方 API 为 `true` | 将 system prompt 与会话中途的 system 消息以 `developer` 而不是 `system` 发送。其他 OpenAI-compatible host 默认用 `system`；显式配置覆盖自动探测。 |
| `requiresNonNullAssistantContent` | `false` | 回放无正文的 assistant 消息（例如只有工具调用）时发送 `""` 而不是 `null`。用于拒绝 null assistant content 的代理。 |
| `requiresToolsWithToolHistory` | `false` | 工具已禁用但历史含 tool call/result 时发送 `"tools": []`。仅用于要求该字段的旧代理（例如 Anthropic 前的旧版 LiteLLM）；vLLM 等服务会拒绝空数组。 |

## 当前目录与 provider 迁移

- **Azure** 的 provider id 统一为 `azure`，同时覆盖 Responses 与 Foundry
  Chat Completions 模型。`azure-openai-responses` 保留为 Responses 的
  **API** id，并继续兼容旧会话回放；新 `auth.json`/`models.json` 条目应使用
  `azure`，旧 provider id 及其已存凭据仍作为兼容别名接受。端点解析支持
  `AZURE_OPENAI_BASE_URL`、`AZURE_OPENAI_RESOURCE_NAME`、
  `AZURE_OPENAI_API_VERSION`，并把 Azure host 规范化到 `/openai/v1`。
  `AZURE_OPENAI_DEPLOYMENT_NAME_MAP=model=deployment,...` 只替换请求载荷中的
  部署名，不改变目录模型 id。
- **Kimi Coding** 现在通过 OpenAI Completions 访问
  `https://api.kimi.com/coding/v1`（device-flow OAuth 不变）。自定义
  `models.json` 条目必须使用 `openai-completions`，且 `baseUrl` 必须带
  `/v1`；K3 的 reasoning-content 回放与提示词缓存行为跟随内嵌模型元数据。
- **Qwen Token Plan** 分为 Team、Team CN、Individual、Individual CN。
  Team/Individual 使用
  `https://token-plan.maas.qwencloudapi.com/compatible-mode/v1`；CN 使用
  `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`。套餐专属 key
  优先于区域共享 key：先读
  `QWEN_TOKEN_PLAN_TEAM[_CN]_API_KEY` 与
  `QWEN_TOKEN_PLAN_INDIVIDUAL[_CN]_API_KEY`，再回退
  `QWEN_TOKEN_PLAN[_CN]_API_KEY`。
- **Gemini** 的版本检测也支持带前缀的模型 id；Gemini 4+ 发送
  `thinkingLevel` 而不是 `thinkingBudget`。Copilot usage 同时接受顶层
  `usage.reasoning_tokens` 与嵌套的 completion details。

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

**区域/端点解析**（TS `shouldUseExplicitBedrockEndpoint`）：自定义
（非标准）base URL 总是原样使用。标准的
`bedrock-runtime.<region>.amazonaws.com` base 只有在未配置
`AWS_REGION` / `AWS_DEFAULT_REGION` 且没有 ambient `AWS_PROFILE`
时才被固定；否则按解析出的 region 重新推导端点，让 env/profile 覆盖
目录默认值。region 优先级：模型 id 中（`:bedrock:` 服务的）ARN →
`AWS_REGION` / `AWS_DEFAULT_REGION` → 被固定的标准 base URL 中的
region → 活动 profile 在 `~/.aws/config` 中的 `region` → 默认
`us-east-1`。base 为空的模型默认 us-east-1 端点（`eu.*` id 为
eu-central-1，与 TS 目录一致）。SDK 默认链的其余部分
（IMDS/ECS/SSO/web-identity）**未实现**。

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
server；**location** 来自 `GOOGLE_CLOUD_LOCATION`，且在 ADC 模式下
必填（TS pi 缺失时会报错）。`GOOGLE_VERTEX_BASE_URL` 覆盖默认端点
host；models.json 中的自定义 `baseUrl` 作为 collection 级 base 使用
（除非已含版本段，否则追加 `/v1`），不支持 `{location}` 模板——与
TS pi 一致。metadata server 的 base URL 可用 `TACK_GCE_METADATA_URL`
覆盖（测试用）。

## 本地 provider（零配置）

- **ollama**：默认 `http://localhost:11434/v1`，`OLLAMA_HOST` 覆盖；启动时
  探测 `/api/tags` 注入运行中服务器的模型。
- **llama.cpp**：默认 `http://localhost:8080/v1`，`LLAMA_CPP_HOST` 覆盖；
  探测 `/v1/models`。

探测约 500ms 超时、失败静默、`--offline` / `TACK_OFFLINE` 时跳过。未知上下文
窗口默认 32k，可用 models.json 按模型覆盖。无需登录或 API key。

## 模型目录

内嵌目录（构建时快照，1179 个模型）转换自已发布的
`@earendil-works/pi-ai` 包数据：上下文窗口、max tokens、推理标志、成本、
逐模型 `compat` 怪癖全部来自目录。`/models refresh` 可从 npm 拉取最新目录
（详见 [features.md](features.zh-CN.md) §模型目录在线刷新）。
