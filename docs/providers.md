# Provider Deep Dive

**English | [简体中文](providers.zh-CN.md)**

Provider details for users and contributors: the auth chain, custom provider
schema, and per-provider adapter characteristics. For the registry overview
and model catalog see the root [README.md](../README.md); for per-provider
env vars and settings keys see [configuration.md](configuration.md).

## Auth resolution order

Every LLM call resolves credentials through `AuthResolver`:

1. `--api-key` CLI flag
2. the provider's `apiKey` in `~/.tack/agent/models.json`
3. the provider's environment variable (same variable names as TS pi, e.g.
   `ZAI_API_KEY`, `MOONSHOT_API_KEY`, `KIMI_API_KEY`)
4. credentials stored in `auth.json` (written by `tack login`)

## OAuth login flows

`tack login --provider <id>` runs the provider's OAuth flow when no
`--api-key` is given (ported from TS pi's `auth/oauth/*`):

| Provider | Flow | Notes |
|---|---|---|
| **anthropic** | Browser PKCE | Claude Pro/Max; loopback port 53692 |
| **openai-codex** | Browser (port 1455) or `--device-code` | ChatGPT Plus/Pro |
| **github-copilot** | device code | Enterprise accounts are asked for their domain |
| **openrouter** | PKCE | Exchanges for a permanent key |
| **kimi-coding** | device flow | |
| **xai** | device flow | |
| **radius** | device flow | |

Browser flows race the loopback callback against manual paste; headless
machines can always paste the redirect URL/code, or use `--device-code`.

Credentials are stored in `auth.json` as
`{ "type": "oauth", access, refresh, expires, ... }` (compatible with TS
pi). Expired tokens are proactively refreshed: a per-process
double-checked lock re-reads after acquiring, so concurrent processes share
one refresh; 15-second timeout; persisted back after refresh.
`tack auth-status` shows each provider's credential type and expiry.

## Custom providers (models.json)

`~/.tack/agent/models.json`, same schema as TS pi:

```jsonc
{
  "providers": {
    "my-provider": {
      "baseUrl": "https://api.example.com/v1",
      "api": "openai-completions",          // adapter type
      "apiKey": "$MY_API_KEY",              // supports $VAR / ${VAR} interpolation
      "headers": { "x-tenant": "…" },
      "compat": { /* per-model / per-provider quirk overrides */ },
      "models": [ { "id": "…", "contextWindow": 128000, /* … */ } ]
    }
  }
}
```

`apiKey` also supports the `!command` form: execute the command and use its
stdout as the key (same behavior as TS pi). Catalogs probed from local
providers are sparsely merged with models.json fields such as
`contextWindow` (models.json wins).

## Amazon Bedrock (`bedrock-converse-stream`)

Implemented without the AWS SDK:

- **SigV4 signing** (`bedrock/sigv4.rs`) — verified against the
  known-answer tests from the AWS docs;
- **AWS event-stream binary frame decoding** (`bedrock/eventstream.rs`) —
  CRC-checked, split/coalesce safe;
- **Converse message conversion** (`bedrock/convert.rs`).

**Credential chain** (by priority):

1. `AWS_BEARER_TOKEN_BEDROCK` / `--api-key` (Bearer mode, no SigV4)
2. `AWS_PROFILE` (reads `~/.aws/credentials`)
3. env var triple `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
   (including `AWS_SESSION_TOKEN`)
4. `AWS_BEDROCK_SKIP_AUTH=1` (fake credentials, local debugging)

**Region/endpoint resolution** (TS `shouldUseExplicitBedrockEndpoint`):
a custom (non-standard) base URL is always used as-is. A standard
`bedrock-runtime.<region>.amazonaws.com` base is pinned only when neither
`AWS_REGION` / `AWS_DEFAULT_REGION` nor an ambient `AWS_PROFILE` is set;
otherwise the endpoint is re-derived from the resolved region so the
env/profile wins over catalog defaults. Region priority: ARN (`:bedrock:`
service) in the model id → `AWS_REGION` / `AWS_DEFAULT_REGION` → region
in a pinned standard base URL → the active profile's `region` in
`~/.aws/config` → default `us-east-1`. Models with an empty base default
to the us-east-1 endpoint (`eu.*` ids to eu-central-1, matching the TS
catalog). The rest of the SDK default chain (IMDS/ECS/SSO/web-identity) is
**not implemented**.

## Google Vertex (`google-vertex`)

Two auth modes:

- **Express API key**: `GOOGLE_CLOUD_API_KEY`;
- **ADC** (full google-auth-library chain):
  1. `GOOGLE_APPLICATION_CREDENTIALS` (service-account JSON → RS256
     JWT-bearer grant);
  2. gcloud common ADC file (`authorized_user` refresh grant, gcloud client
     constants);
  3. GCE metadata server (`Metadata-Flavor: Google`, probed with sub-second
     timeout, fast failure off GCP).

Tokens are cached until near expiry. **project** comes from
`GOOGLE_CLOUD_PROJECT` / `GCLOUD_PROJECT` / SA `project_id` / metadata
server; **location** comes from `GOOGLE_CLOUD_LOCATION` and is required in
ADC mode (TS pi errors without it). `GOOGLE_VERTEX_BASE_URL` overrides the
default endpoint host; a custom `baseUrl` in models.json is used as a
collection-scope base (`/v1` appended unless it already carries a version
segment) and `{location}` templating is not supported — same as TS pi.
The metadata server's base URL can be overridden with
`TACK_GCE_METADATA_URL` (for tests).

## Local providers (zero config)

- **ollama**: default `http://localhost:11434/v1`, overridden by
  `OLLAMA_HOST`; probes `/api/tags` at startup to inject the models of a
  running server.
- **llama.cpp**: default `http://localhost:8080/v1`, overridden by
  `LLAMA_CPP_HOST`; probes `/v1/models`.

Probing has a ~500ms timeout, fails silently, and is skipped under
`--offline` / `TACK_OFFLINE`. Unknown context windows default to 32k,
overridable per model in models.json. No login or API key needed.

## Model catalog

The embedded catalog (build-time snapshot, 1130 models) is converted from
the published `@earendil-works/pi-ai` package data: context windows, max
tokens, reasoning flags, costs, and per-model `compat` quirks all come from
the catalog. `/models refresh` can pull the latest catalog from npm
(details: [features.md](features.md), the section on online catalog refresh).
