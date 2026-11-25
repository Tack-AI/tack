//! API key resolution from environment variables.
//! Lean port of `packages/ai/src/env-api-keys.ts`.

/// Known provider → env var names, in priority order.
fn env_var_names(provider: &str) -> Vec<&'static str> {
    match provider {
        "anthropic" => vec!["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        "openai" => vec!["OPENAI_API_KEY"],
        "openai-codex" | "openai-codex-responses" => vec!["OPENAI_API_KEY", "CODEX_API_KEY"],
        "azure" | "azure-openai-responses" => vec!["AZURE_OPENAI_API_KEY"],
        // Per-plan keys come first so Individual credentials never select a
        // Team-only catalog; the regional shared keys remain fallbacks.
        "qwen-token-plan" => vec!["QWEN_TOKEN_PLAN_TEAM_API_KEY", "QWEN_TOKEN_PLAN_API_KEY"],
        "qwen-token-plan-cn" => {
            vec![
                "QWEN_TOKEN_PLAN_TEAM_CN_API_KEY",
                "QWEN_TOKEN_PLAN_CN_API_KEY",
            ]
        }
        "qwen-token-plan-individual" => vec![
            "QWEN_TOKEN_PLAN_INDIVIDUAL_API_KEY",
            "QWEN_TOKEN_PLAN_API_KEY",
        ],
        "qwen-token-plan-individual-cn" => vec![
            "QWEN_TOKEN_PLAN_INDIVIDUAL_CN_API_KEY",
            "QWEN_TOKEN_PLAN_CN_API_KEY",
        ],
        "google" => vec!["GEMINI_API_KEY"],
        "google-vertex" => vec!["GOOGLE_CLOUD_API_KEY"],
        "deepseek" => vec!["DEEPSEEK_API_KEY"],
        "openrouter" => vec!["OPENROUTER_API_KEY"],
        "groq" => vec!["GROQ_API_KEY"],
        "xai" => vec!["XAI_API_KEY"],
        "mistral" => vec!["MISTRAL_API_KEY"],
        "moonshotai" | "moonshotai-cn" => vec!["MOONSHOT_API_KEY"],
        "zai" => vec!["ZAI_API_KEY"],
        "zai-coding-cn" => vec!["ZAI_CODING_CN_API_KEY"],
        "together" => vec!["TOGETHER_API_KEY"],
        "fireworks" => vec!["FIREWORKS_API_KEY"],
        "cerebras" => vec!["CEREBRAS_API_KEY"],
        "nvidia" => vec!["NVIDIA_API_KEY"],
        "huggingface" => vec!["HF_TOKEN"],
        // kimi-coding talks to Moonshot's api.kimi.com — never fall back to
        // another vendor's key here (an Anthropic key would be leaked to
        // Moonshot's server).
        "kimi-coding" => vec!["KIMI_API_KEY"],
        "opencode" | "opencode-go" => vec!["OPENCODE_API_KEY"],
        _ => vec![],
    }
}

/// Generic fallback: `my-provider` → `MY_PROVIDER_API_KEY`.
fn generic_env_var(provider: &str) -> String {
    let normalized: String = provider
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{normalized}_API_KEY")
}

/// Resolve an API key for a provider from the process environment.
/// Known provider mappings first, then the generic `{PROVIDER}_API_KEY`.
pub fn get_env_api_key(provider: &str) -> Option<String> {
    for name in env_var_names(provider) {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    let generic = generic_env_var(provider);
    if !env_var_names(provider).contains(&generic.as_str())
        && let Ok(value) = std::env::var(&generic)
        && !value.is_empty()
    {
        return Some(value);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kimi Coding is Moonshot-hosted; never use another vendor's key here.
    #[test]
    fn kimi_coding_never_falls_back_to_anthropic_key() {
        let names = env_var_names("kimi-coding");
        assert_eq!(names, vec!["KIMI_API_KEY"]);
        assert!(!names.contains(&"ANTHROPIC_API_KEY"));
    }

    /// Same-vendor fallbacks stay intact.
    #[test]
    fn same_vendor_fallbacks_are_kept() {
        assert_eq!(
            env_var_names("anthropic"),
            vec!["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"]
        );
        assert_eq!(
            env_var_names("openai-codex"),
            vec!["OPENAI_API_KEY", "CODEX_API_KEY"]
        );
    }

    #[test]
    fn azure_and_qwen_env_key_priorities_match_upstream() {
        assert_eq!(env_var_names("azure"), vec!["AZURE_OPENAI_API_KEY"]);
        assert_eq!(
            env_var_names("qwen-token-plan"),
            vec!["QWEN_TOKEN_PLAN_TEAM_API_KEY", "QWEN_TOKEN_PLAN_API_KEY"]
        );
        assert_eq!(
            env_var_names("qwen-token-plan-cn"),
            vec![
                "QWEN_TOKEN_PLAN_TEAM_CN_API_KEY",
                "QWEN_TOKEN_PLAN_CN_API_KEY"
            ]
        );
        assert_eq!(
            env_var_names("qwen-token-plan-individual"),
            vec![
                "QWEN_TOKEN_PLAN_INDIVIDUAL_API_KEY",
                "QWEN_TOKEN_PLAN_API_KEY"
            ]
        );
        assert_eq!(
            env_var_names("qwen-token-plan-individual-cn"),
            vec![
                "QWEN_TOKEN_PLAN_INDIVIDUAL_CN_API_KEY",
                "QWEN_TOKEN_PLAN_CN_API_KEY"
            ]
        );
    }

    #[test]
    fn generic_fallback_derives_from_provider_id() {
        assert_eq!(generic_env_var("my-provider"), "MY_PROVIDER_API_KEY");
        assert_eq!(generic_env_var("kimi-coding"), "KIMI_CODING_API_KEY");
    }
}
