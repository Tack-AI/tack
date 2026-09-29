//! Built-in provider registry, aligned with TypeScript pi's
//! `packages/ai/src/providers/` (~45 providers). Model catalogs are embedded
//! from `catalog.json`, converted from the published `@earendil-works/pi-ai`
//! generated data (see tack/scripts/ or the README for regeneration).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use serde::Deserialize;

use crate::types::Model;

/// Static definition of a built-in provider (the TS factory files).
#[derive(Clone, Copy, Debug)]
pub struct BuiltinProviderDef {
    pub id: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    /// Wire protocol (`api` kind).
    pub api: &'static str,
    /// Environment variables checked for API keys, in priority order.
    pub env_keys: &'static [&'static str],
}

/// The built-in providers, in the same order as TS pi's `providers/all.ts`.
/// Providers whose api has no Rust adapter yet are listed for id/catalog
/// parity but `provider_for` returns None for their models.
pub const BUILTIN_PROVIDERS: &[BuiltinProviderDef] = &[
    BuiltinProviderDef {
        id: "amazon-bedrock",
        name: "Amazon Bedrock",
        base_url: "",
        api: "bedrock-converse-stream",
        env_keys: &["AWS_BEARER_TOKEN_BEDROCK"],
    },
    BuiltinProviderDef {
        id: "ant-ling",
        name: "Ant Ling",
        base_url: "https://api.ant-ling.com/v1",
        api: "openai-completions",
        env_keys: &["ANT_LING_API_KEY"],
    },
    BuiltinProviderDef {
        id: "anthropic",
        name: "Anthropic",
        base_url: "https://api.anthropic.com",
        api: "anthropic-messages",
        env_keys: &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
    },
    BuiltinProviderDef {
        id: "azure-openai-responses",
        name: "Azure OpenAI Responses",
        base_url: "",
        api: "azure-openai-responses",
        env_keys: &["AZURE_OPENAI_API_KEY"],
    },
    BuiltinProviderDef {
        id: "baseten",
        name: "Baseten",
        base_url: "https://inference.baseten.co/v1",
        api: "openai-completions",
        env_keys: &["BASETEN_API_KEY"],
    },
    BuiltinProviderDef {
        id: "cerebras",
        name: "Cerebras",
        base_url: "https://api.cerebras.ai/v1",
        api: "openai-completions",
        env_keys: &["CEREBRAS_API_KEY"],
    },
    BuiltinProviderDef {
        id: "cloudflare-ai-gateway",
        name: "Cloudflare AI Gateway",
        base_url: "",
        api: "anthropic-messages",
        env_keys: &["CLOUDFLARE_API_KEY"],
    },
    BuiltinProviderDef {
        id: "cloudflare-workers-ai",
        name: "Cloudflare Workers AI",
        base_url: "",
        api: "openai-completions",
        env_keys: &["CLOUDFLARE_API_KEY"],
    },
    BuiltinProviderDef {
        id: "codebuddy",
        name: "CodeBuddy (CLI)",
        base_url: "",
        api: crate::codebuddy::CODEBUDDY_API,
        // No env keys: auth is CLI-side (`codebuddy login`).
        env_keys: &[],
    },
    BuiltinProviderDef {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com",
        api: "openai-completions",
        env_keys: &["DEEPSEEK_API_KEY"],
    },
    BuiltinProviderDef {
        id: "fireworks",
        name: "Fireworks",
        base_url: "https://api.fireworks.ai/inference",
        api: "anthropic-messages",
        env_keys: &["FIREWORKS_API_KEY"],
    },
    BuiltinProviderDef {
        id: "github-copilot",
        name: "GitHub Copilot",
        base_url: "https://api.individual.githubcopilot.com",
        api: "anthropic-messages",
        env_keys: &["COPILOT_GITHUB_TOKEN"],
    },
    BuiltinProviderDef {
        id: "google",
        name: "Google",
        base_url: "https://generativelanguage.googleapis.com",
        api: "google-generative-ai",
        env_keys: &["GEMINI_API_KEY"],
    },
    BuiltinProviderDef {
        id: "google-vertex",
        name: "Google Vertex",
        base_url: "",
        api: "google-vertex",
        env_keys: &["GOOGLE_CLOUD_API_KEY"],
    },
    BuiltinProviderDef {
        id: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        api: "openai-completions",
        env_keys: &["GROQ_API_KEY"],
    },
    BuiltinProviderDef {
        id: "huggingface",
        name: "Hugging Face",
        base_url: "https://router.huggingface.co/v1",
        api: "openai-completions",
        env_keys: &["HF_TOKEN"],
    },
    BuiltinProviderDef {
        id: "kimi-coding",
        name: "Kimi For Coding",
        base_url: "https://api.kimi.com/coding",
        api: "anthropic-messages",
        // Moonshot-hosted despite the Anthropic wire format: no
        // ANTHROPIC_API_KEY fallback (it would leak the key to api.kimi.com).
        env_keys: &["KIMI_API_KEY"],
    },
    BuiltinProviderDef {
        id: "llama.cpp",
        name: "llama.cpp (local)",
        base_url: "http://localhost:8080/v1",
        api: "openai-completions",
        env_keys: &[],
    },
    BuiltinProviderDef {
        id: "minimax",
        name: "MiniMax",
        base_url: "https://api.minimax.io/anthropic",
        api: "anthropic-messages",
        env_keys: &["MINIMAX_API_KEY"],
    },
    BuiltinProviderDef {
        id: "minimax-cn",
        name: "MiniMax (China)",
        base_url: "https://api.minimaxi.com/anthropic",
        api: "anthropic-messages",
        env_keys: &["MINIMAX_CN_API_KEY"],
    },
    BuiltinProviderDef {
        id: "mistral",
        name: "Mistral",
        base_url: "https://api.mistral.ai",
        api: "mistral-conversations",
        env_keys: &["MISTRAL_API_KEY"],
    },
    BuiltinProviderDef {
        id: "moonshotai",
        name: "Moonshot AI",
        base_url: "https://api.moonshot.ai/v1",
        api: "openai-completions",
        env_keys: &["MOONSHOT_API_KEY"],
    },
    BuiltinProviderDef {
        id: "moonshotai-cn",
        name: "Moonshot AI (China)",
        base_url: "https://api.moonshot.cn/v1",
        api: "openai-completions",
        env_keys: &["MOONSHOT_API_KEY"],
    },
    BuiltinProviderDef {
        id: "nvidia",
        name: "NVIDIA",
        base_url: "https://integrate.api.nvidia.com/v1",
        api: "openai-completions",
        env_keys: &["NVIDIA_API_KEY"],
    },
    BuiltinProviderDef {
        id: "ollama",
        name: "Ollama (local)",
        base_url: "http://localhost:11434/v1",
        api: "openai-completions",
        env_keys: &[],
    },
    BuiltinProviderDef {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        api: "openai-responses",
        env_keys: &["OPENAI_API_KEY"],
    },
    BuiltinProviderDef {
        id: "openai-codex",
        name: "OpenAI Codex",
        base_url: "https://chatgpt.com/backend-api",
        api: "openai-codex-responses",
        env_keys: &["OPENAI_API_KEY", "CODEX_API_KEY"],
    },
    BuiltinProviderDef {
        id: "opencode",
        name: "OpenCode Zen",
        base_url: "https://opencode.ai/zen/v1",
        api: "anthropic-messages",
        env_keys: &["OPENCODE_API_KEY"],
    },
    BuiltinProviderDef {
        id: "opencode-go",
        name: "OpenCode Go",
        base_url: "https://opencode.ai/zen/go/v1",
        api: "anthropic-messages",
        env_keys: &["OPENCODE_API_KEY"],
    },
    BuiltinProviderDef {
        id: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        api: "openai-completions",
        env_keys: &["OPENROUTER_API_KEY"],
    },
    BuiltinProviderDef {
        id: "qwen-token-plan",
        name: "Qwen Token Plan",
        base_url: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        api: "openai-completions",
        env_keys: &["QWEN_TOKEN_PLAN_API_KEY"],
    },
    BuiltinProviderDef {
        id: "qwen-token-plan-cn",
        name: "Qwen Token Plan (China)",
        base_url: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        api: "openai-completions",
        env_keys: &["QWEN_TOKEN_PLAN_CN_API_KEY"],
    },
    BuiltinProviderDef {
        id: "qwen-token-plan-individual",
        name: "Qwen Token Plan (Individual)",
        base_url: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        api: "openai-completions",
        env_keys: &["QWEN_TOKEN_PLAN_API_KEY"],
    },
    BuiltinProviderDef {
        id: "radius",
        name: "Radius",
        base_url: "",
        api: crate::provider::TACK_MESSAGES_API,
        env_keys: &["RADIUS_API_KEY"],
    },
    BuiltinProviderDef {
        id: "together",
        name: "Together AI",
        base_url: "https://api.together.ai/v1",
        api: "openai-completions",
        env_keys: &["TOGETHER_API_KEY"],
    },
    BuiltinProviderDef {
        id: "vercel-ai-gateway",
        name: "Vercel AI Gateway",
        base_url: "https://ai-gateway.vercel.sh",
        api: "anthropic-messages",
        env_keys: &["AI_GATEWAY_API_KEY"],
    },
    BuiltinProviderDef {
        id: "xai",
        name: "xAI",
        base_url: "https://api.x.ai/v1",
        api: "openai-responses",
        env_keys: &["XAI_API_KEY"],
    },
    BuiltinProviderDef {
        id: "xiaomi",
        name: "Xiaomi MiMo",
        base_url: "https://api.xiaomimimo.com/v1",
        api: "openai-completions",
        env_keys: &["XIAOMI_API_KEY"],
    },
    BuiltinProviderDef {
        id: "xiaomi-token-plan-ams",
        name: "Xiaomi MiMo Token Plan (Amsterdam)",
        base_url: "https://token-plan-ams.xiaomimimo.com/v1",
        api: "openai-completions",
        env_keys: &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
    },
    BuiltinProviderDef {
        id: "xiaomi-token-plan-cn",
        name: "Xiaomi MiMo Token Plan (China)",
        base_url: "https://token-plan-cn.xiaomimimo.com/v1",
        api: "openai-completions",
        env_keys: &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
    },
    BuiltinProviderDef {
        id: "xiaomi-token-plan-sgp",
        name: "Xiaomi MiMo Token Plan (Singapore)",
        base_url: "https://token-plan-sgp.xiaomimimo.com/v1",
        api: "openai-completions",
        env_keys: &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
    },
    BuiltinProviderDef {
        id: "zai",
        name: "Z.AI Coding Plan",
        base_url: "https://api.z.ai/api/coding/paas/v4",
        api: "openai-completions",
        env_keys: &["ZAI_API_KEY"],
    },
    BuiltinProviderDef {
        id: "zai-coding-cn",
        name: "Z.AI Coding Plan (China)",
        base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        api: "openai-completions",
        env_keys: &["ZAI_CODING_CN_API_KEY"],
    },
];

/// Look up a built-in provider definition by id.
pub fn builtin_provider(id: &str) -> Option<&'static BuiltinProviderDef> {
    BUILTIN_PROVIDERS.iter().find(|p| p.id == id)
}

// ---------------------------------------------------------------------------
// Embedded model catalog
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CatalogProvider {
    #[allow(dead_code)]
    api: String,
    models: Vec<Model>,
}

/// Models discovered from locally running servers (Ollama, llama.cpp) by
/// [`crate::local_providers`]. Consulted before the embedded catalog so a
/// live discovery always wins over stale/embedded data for that provider.
static LOCAL_MODELS: RwLock<BTreeMap<String, Arc<[Model]>>> = RwLock::new(BTreeMap::new());

static CATALOG: OnceLock<()> = OnceLock::new();
static CATALOG_MODELS: RwLock<BTreeMap<String, Arc<[Model]>>> = RwLock::new(BTreeMap::new());
static CATALOG_OVERRIDDEN: RwLock<Vec<String>> = RwLock::new(Vec::new());

// Lock-poisoning-tolerant accessors: the catalog is never left
// inconsistent (writes are wholesale), so recovering from a poisoned lock
// is safe.
fn read_catalog<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write_catalog<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

fn ensure_catalog() {
    CATALOG.get_or_init(|| {
        let raw: BTreeMap<String, CatalogProvider> =
            serde_json::from_str(include_str!("../catalog.json"))
                .expect("embedded catalog.json is valid");
        let mut map = write_catalog(&CATALOG_MODELS);
        if map.is_empty() {
            for (k, v) in raw {
                map.insert(k, v.models.into_iter().collect::<Arc<[Model]>>());
            }
        }
    });
}

/// Replace the model lists of the given providers wholesale (a refreshed
/// catalog from a newer upstream). Providers not in the map keep the
/// embedded list. Returns the previous override provider count.
pub fn install_catalog_override(override_catalog: BTreeMap<String, Vec<Model>>) -> usize {
    ensure_catalog();
    let mut models = write_catalog(&CATALOG_MODELS);
    let mut overridden = write_catalog(&CATALOG_OVERRIDDEN);
    let prev = overridden.len();
    for (provider, list) in override_catalog {
        if !overridden.contains(&provider) {
            overridden.push(provider.clone());
        }
        models.insert(provider, list.into_iter().collect());
    }
    prev
}

/// Providers whose catalog currently comes from an override (0 = embedded only).
pub fn catalog_override_count() -> usize {
    ensure_catalog();
    read_catalog(&CATALOG_OVERRIDDEN).len()
}

/// Parse a catalog JSON document (same shape as the embedded catalog.json).
pub fn parse_catalog_json(raw: &str) -> Result<BTreeMap<String, Vec<Model>>, String> {
    let parsed: BTreeMap<String, CatalogProvider> =
        serde_json::from_str(raw).map_err(|e| format!("invalid catalog JSON: {e}"))?;
    Ok(parsed.into_iter().map(|(k, v)| (k, v.models)).collect())
}

/// Install (replace) a locally discovered model list for a provider. Called
/// by [`crate::local_providers::refresh`] after probing localhost servers.
pub fn install_local_models(provider_id: &str, models: Vec<Model>) {
    write_catalog(&LOCAL_MODELS).insert(provider_id.to_string(), models.into_iter().collect());
}

/// All catalog models for a provider (empty if none).
pub fn builtin_models(provider_id: &str) -> Arc<[Model]> {
    // Live local discovery (Ollama, llama.cpp) takes precedence over the
    // embedded catalog for its provider.
    if let Some(models) = read_catalog(&LOCAL_MODELS).get(provider_id) {
        return models.clone();
    }
    ensure_catalog();
    CATALOG_MODELS
        .read()
        .map(|m| m.get(provider_id).cloned())
        .unwrap_or_else(|e| e.into_inner().get(provider_id).cloned())
        .unwrap_or_else(|| Arc::from(Vec::new().into_boxed_slice()))
}

/// A specific catalog model.
pub fn builtin_model(provider_id: &str, model_id: &str) -> Option<Model> {
    builtin_models(provider_id)
        .iter()
        .find(|m| m.id == model_id)
        .cloned()
}

/// The provider's default model: the first catalog entry (catalogs list the
/// flagship first), falling back to a bare model from the provider def.
pub fn default_model_for(provider_id: &str) -> Option<Model> {
    if let Some(model) = builtin_models(provider_id).first() {
        return Some(model.clone());
    }
    let def = builtin_provider(provider_id)?;
    if def.api.is_empty() || def.base_url.is_empty() {
        return None;
    }
    Some(Model {
        id: String::new(),
        name: def.name.to_string(),
        api: def.api.to_string(),
        provider: def.id.to_string(),
        base_url: def.base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![crate::types::InputKind::Text],
        cost: Default::default(),
        context_window: 0,
        max_tokens: 0,
        sampling_params: None,
        headers: None,
        compat: None,
    })
}

// ---------------------------------------------------------------------------
// Custom providers from models.json (same schema as TS pi)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomProvidersFile {
    providers: BTreeMap<String, CustomProvider>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomProvider {
    base_url: String,
    api: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    compat: Option<serde_json::Value>,
    models: Vec<CustomModel>,
}

#[derive(Clone, Debug, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub input: Option<Vec<crate::types::InputKind>>,
    #[serde(default)]
    pub cost: Option<crate::types::ModelCost>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub compat: Option<serde_json::Value>,
    #[serde(default)]
    pub sampling_params: Option<BTreeMap<String, serde_json::Value>>,
}

/// A custom provider from models.json, with its API key expression resolved.
#[derive(Clone, Debug)]
pub struct CustomProviderModels {
    pub id: String,
    pub api_key: Option<String>,
    pub models: Vec<Model>,
    /// Sparse per-model overrides (only fields set in models.json), parallel
    /// to `models` — used to patch built-in catalog entries on merge.
    pub sparse: Vec<CustomModelSparse>,
}

/// Sparse model fields as written in models.json.
#[derive(Clone, Debug, Default)]
pub struct CustomModelSparse {
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub input: Option<Vec<crate::types::InputKind>>,
    pub cost: Option<crate::types::ModelCost>,
    pub context_window: Option<u32>,
    pub max_tokens: Option<u32>,
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<serde_json::Value>,
    pub sampling_params: Option<BTreeMap<String, serde_json::Value>>,
}

impl CustomModelSparse {
    /// Patch a built-in catalog model in place (only explicitly-set fields).
    pub fn apply_to(&self, model: &mut Model) {
        if let Some(v) = &self.name {
            model.name = v.clone();
        }
        if let Some(v) = self.reasoning {
            model.reasoning = v;
        }
        if let Some(v) = &self.input {
            model.input = v.clone();
        }
        if let Some(v) = &self.cost {
            model.cost = v.clone();
        }
        if let Some(v) = self.context_window {
            model.context_window = v;
        }
        if let Some(v) = self.max_tokens {
            model.max_tokens = v;
        }
        if let Some(v) = &self.thinking_level_map {
            model.thinking_level_map = Some(v.clone());
        }
        if let Some(v) = &self.headers {
            model.headers = Some(v.clone());
        }
        if let Some(v) = &self.compat {
            model.compat = Some(v.clone());
        }
        if let Some(v) = &self.sampling_params {
            model.sampling_params = Some(v.clone());
        }
    }
}

/// `!cmd` apiKey helper with a hard deadline: a hanging credential
/// helper (locked password-manager CLI, blocked on a prompt) must not
/// block the caller — often an async worker thread — forever.
fn command_stdout_with_timeout(cmd: &str) -> Option<String> {
    use std::io::Read as _;
    let mut command = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
    command
        .args(if cfg!(windows) {
            vec!["/C", cmd]
        } else {
            vec!["-c", cmd]
        })
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped());
    let mut child = command.spawn().ok()?;
    let mut stdout_pipe = child.stdout.take()?;
    // Drain on a helper thread: a full pipe buffer would otherwise block
    // the child mid-write and it would never exit.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let bytes = reader.join().unwrap_or_default();
                return status
                    .success()
                    .then(|| String::from_utf8_lossy(&bytes).trim().to_string());
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(2)),
            Err(_) => return None,
        }
    }
}

/// Resolve an apiKey config value: `!cmd` executes a command, `$VAR` /
/// `${VAR}` interpolate environment variables (`$$` → `$`, `$!` → `!`).
fn resolve_config_value(value: &str) -> Option<String> {
    if let Some(cmd) = value.strip_prefix('!') {
        return command_stdout_with_timeout(cmd);
    }
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('$') => {
                out.push('$');
                chars.next();
            }
            Some('!') => {
                out.push('!');
                chars.next();
            }
            Some('{') => {
                chars.next();
                let mut name = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    name.push(c);
                }
                if let Ok(v) = std::env::var(&name) {
                    out.push_str(&v);
                }
            }
            _ => {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if name.is_empty() {
                    out.push('$');
                } else if let Ok(v) = std::env::var(&name) {
                    out.push_str(&v);
                }
            }
        }
    }
    Some(out)
}

/// Load custom providers from `<agent_dir>/models.json` (TS pi schema).
pub fn load_custom_providers(agent_dir: &std::path::Path) -> Vec<CustomProviderModels> {
    let path = agent_dir.join("models.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let file: CustomProvidersFile = match serde_json::from_str(&content) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("ignoring malformed models.json {}: {e}", path.display());
            return Vec::new();
        }
    };

    file.providers
        .into_iter()
        .map(|(id, provider)| {
            build_custom_provider(
                id,
                provider.api,
                provider.base_url,
                provider.api_key.as_deref().and_then(resolve_config_value),
                provider.headers,
                provider.compat,
                provider.models,
            )
        })
        .collect()
}

fn build_custom_provider(
    id: String,
    api: String,
    base_url: String,
    api_key: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    compat: Option<serde_json::Value>,
    provider_models: Vec<CustomModel>,
) -> CustomProviderModels {
    // Normalize api-kind aliases (`pi-messages` → `tack-messages`) so models
    // carry the canonical name into sessions and cache keys.
    let api = crate::provider::canonical_api_kind(&api).to_string();
    let mut models = Vec::new();
    let mut sparse = Vec::new();
    for m in provider_models {
        sparse.push(CustomModelSparse {
            name: m.name.clone(),
            reasoning: m.reasoning,
            input: m.input.clone(),
            cost: m.cost.clone(),
            context_window: m.context_window,
            max_tokens: m.max_tokens,
            thinking_level_map: m.thinking_level_map.clone(),
            headers: m.headers.clone(),
            compat: m.compat.clone(),
            sampling_params: m.sampling_params.clone(),
        });
        models.push(Model {
            id: m.id.clone(),
            name: m.name.unwrap_or_else(|| m.id.clone()),
            api: api.clone(),
            provider: id.clone(),
            base_url: base_url.clone(),
            reasoning: m.reasoning.unwrap_or(false),
            thinking_level_map: m.thinking_level_map,
            input: m
                .input
                .unwrap_or_else(|| vec![crate::types::InputKind::Text]),
            cost: m.cost.unwrap_or_default(),
            context_window: m.context_window.unwrap_or(0),
            max_tokens: m.max_tokens.unwrap_or(0),
            sampling_params: m.sampling_params,
            headers: m.headers.or_else(|| headers.clone()),
            compat: m.compat.or_else(|| compat.clone()),
        });
    }
    CustomProviderModels {
        id,
        api_key,
        models,
        sparse,
    }
}

// ---------------------------------------------------------------------------
// Runtime provider registry (tack-ext `provider.register`)
// ---------------------------------------------------------------------------

/// A provider registered at runtime by a plugin (same shape as a models.json
/// provider entry, plus the optional `bridge` flag).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeProviderSpec {
    pub id: String,
    /// Ignored for bridge providers (no HTTP endpoint exists).
    #[serde(default)]
    pub base_url: String,
    /// Wire protocol id (`openai-completions`, `anthropic-messages`, …).
    /// Must be absent/empty for bridge providers: every model is assigned
    /// the reserved [`crate::provider_bridge::EXT_PROVIDER_BRIDGE_API`]
    /// kind; a conflicting explicit api is a registration error.
    #[serde(default)]
    pub api: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Env var naming the API key. **Ignored** for runtime (plugin-
    /// registered) providers: resolving a plugin-chosen variable from the
    /// host environment would let a plugin harvest the host's credentials
    /// and — paired with a plugin-chosen `baseUrl` — exfiltrate them.
    /// Runtime providers must carry their key explicitly in `apiKey` (or
    /// none). The field stays in the schema for wire tolerance.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub compat: Option<serde_json::Value>,
    /// `true` = the registering plugin serves inference itself via the v3
    /// `provider/stream` bridge; `baseUrl`/`apiKey`/`headers` are ignored.
    #[serde(default)]
    pub bridge: Option<bool>,
    pub models: Vec<CustomModel>,
}

static RUNTIME_PROVIDERS: std::sync::Mutex<Vec<CustomProviderModels>> =
    std::sync::Mutex::new(Vec::new());

/// Register (or replace) a runtime provider. Models become selectable via
/// `/model`, resolvable via resolve_model, and callable through the api kind's
/// adapter. Bridge providers (`bridge: true`) get the reserved
/// [`crate::provider_bridge::EXT_PROVIDER_BRIDGE_API`] kind on every model and
/// resolve to the plugin's serving connection instead of an HTTP adapter.
pub fn register_runtime_provider(spec: RuntimeProviderSpec) -> Result<(), String> {
    if spec.id.is_empty() {
        return Err("provider.register needs an id".to_string());
    }
    if spec.models.is_empty() {
        return Err(format!("provider {} registers no models", spec.id));
    }
    // Built-in ids are owned by the host: a runtime provider shadowing one
    // would override the built-in's baseUrl while `resolve_api_key` keeps
    // handing it the user's stored credentials — plugin-steered
    // exfiltration (the same attack catalog-supplied baseUrls are guarded
    // against). Applies to plain and bridge specs alike.
    if let Some(def) = builtin_provider(&spec.id) {
        return Err(format!(
            "provider.register id {:?} collides with built-in provider {:?} ({}): runtime providers must not shadow built-ins",
            spec.id, def.id, def.name
        ));
    }
    let bridge = spec.bridge == Some(true);
    let (api, base_url) = if bridge {
        if !spec.api.is_empty() && spec.api != crate::provider_bridge::EXT_PROVIDER_BRIDGE_API {
            return Err(format!(
                "bridge provider {} must not declare api {:?}: every model is assigned the reserved {} kind",
                spec.id,
                spec.api,
                crate::provider_bridge::EXT_PROVIDER_BRIDGE_API
            ));
        }
        (
            crate::provider_bridge::EXT_PROVIDER_BRIDGE_API.to_string(),
            String::new(),
        )
    } else {
        if spec.api.is_empty() || spec.base_url.is_empty() {
            return Err("provider.register needs id, api, and baseUrl".to_string());
        }
        (spec.api.clone(), spec.base_url.clone())
    };
    if spec.api_key.is_none()
        && let Some(var) = &spec.api_key_env
    {
        tracing::warn!(
            "runtime provider {}: apiKeyEnv ({var}) is ignored — plugin-registered providers must carry their key in apiKey",
            spec.id
        );
    }
    let provider = build_custom_provider(
        spec.id.clone(),
        api,
        base_url,
        spec.api_key,
        spec.headers,
        spec.compat,
        spec.models,
    );
    let mut registry = RUNTIME_PROVIDERS
        .lock()
        .expect("runtime providers poisoned");
    registry.retain(|p| p.id != spec.id);
    registry.push(provider);
    Ok(())
}

/// Remove a runtime-registered provider (the serving plugin is gone).
/// Resolution after removal hits the unknown-provider path — the same
/// behavior as a native provider whose CLI disappeared.
pub fn unregister_runtime_provider(id: &str) {
    RUNTIME_PROVIDERS
        .lock()
        .expect("runtime providers poisoned")
        .retain(|p| p.id != id);
}

/// All runtime-registered providers (plugin-registered).
pub fn runtime_providers() -> Vec<CustomProviderModels> {
    RUNTIME_PROVIDERS
        .lock()
        .expect("runtime providers poisoned")
        .clone()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(unsafe_code)] // env-var fixtures (std::env::set_var is unsafe in edition 2024)
    use super::*;

    fn runtime_spec(id: &str) -> RuntimeProviderSpec {
        RuntimeProviderSpec {
            id: id.to_string(),
            base_url: "http://localhost:9999".to_string(),
            api: "openai-completions".to_string(),
            api_key: None,
            api_key_env: None,
            headers: None,
            compat: None,
            bridge: None,
            models: vec![CustomModel {
                id: "m1".to_string(),
                name: None,
                reasoning: None,
                input: None,
                cost: None,
                context_window: Some(128_000),
                max_tokens: Some(4096),
                thinking_level_map: None,
                headers: None,
                compat: None,
                sampling_params: None,
            }],
        }
    }

    /// A runtime provider id colliding with a built-in is rejected — for
    /// plain and bridge specs alike (a shadow would inherit the user's
    /// stored credentials for the built-in while steering requests to a
    /// plugin-chosen endpoint).
    #[test]
    fn runtime_provider_cannot_shadow_a_builtin() {
        for bridge in [false, true] {
            let mut spec = runtime_spec("anthropic");
            if bridge {
                spec.bridge = Some(true);
                spec.api = String::new();
                spec.base_url = String::new();
            }
            let err = register_runtime_provider(spec).unwrap_err();
            assert!(err.contains("anthropic"), "{err}");
            assert!(err.contains("built-in"), "{err}");
        }
        assert!(
            !runtime_providers().iter().any(|p| p.id == "anthropic"),
            "the built-in id never enters the runtime registry"
        );
    }

    /// `apiKeyEnv` is not resolved from the host environment: runtime
    /// providers carry their key explicitly in `apiKey` or not at all.
    #[test]
    fn runtime_provider_api_key_env_is_not_resolved_from_the_host_env() {
        let id = "test-unit-rt-env-ignored";
        unsafe { std::env::set_var("TACK_UNIT_RT_KEY_IGNORED", "sk-should-not-leak") };
        let mut spec = runtime_spec(id);
        spec.api_key_env = Some("TACK_UNIT_RT_KEY_IGNORED".to_string());
        register_runtime_provider(spec).unwrap();
        let registered = runtime_providers()
            .into_iter()
            .find(|p| p.id == id)
            .expect("registered");
        assert_eq!(registered.api_key, None, "host env must not be read");
        unregister_runtime_provider(id);
        unsafe { std::env::remove_var("TACK_UNIT_RT_KEY_IGNORED") };
    }

    /// models.json with the legacy `pi-messages` alias loads with the
    /// canonical `tack-messages` api kind (TS pi config parity).
    #[test]
    fn models_json_pi_messages_alias_normalizes_to_tack_messages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            serde_json::json!({
                "providers": {
                    "radius": {
                        "baseUrl": "https://radius.example.com/v1",
                        "api": "pi-messages",
                        "apiKey": "sk-test",
                        "models": [{"id": "r1"}]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let loaded = load_custom_providers(dir.path());
        assert_eq!(loaded.len(), 1);
        assert!(
            loaded[0]
                .models
                .iter()
                .all(|m| m.api == crate::provider::TACK_MESSAGES_API),
            "models: {:?}",
            loaded[0].models
        );
    }

    /// The canonical name loads untouched.
    #[test]
    fn models_json_tack_messages_loads_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            serde_json::json!({
                "providers": {
                    "radius": {
                        "baseUrl": "https://radius.example.com/v1",
                        "api": "tack-messages",
                        "apiKey": "sk-test",
                        "models": [{"id": "r1"}]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let loaded = load_custom_providers(dir.path());
        assert_eq!(loaded[0].models[0].api, "tack-messages");
    }
}
