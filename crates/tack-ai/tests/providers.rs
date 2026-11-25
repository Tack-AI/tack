//! Provider registry parity tests (TS pi built-ins + embedded catalog).
#![allow(clippy::unwrap_used)]

use tack_ai::providers::*;

#[test]
fn registry_covers_ts_builtin_providers() {
    // The full TS pi built-in set (packages/ai/src/providers/all.ts).
    let expected = [
        "amazon-bedrock",
        "ant-ling",
        "anthropic",
        "azure",
        "baseten",
        "cerebras",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
        "deepseek",
        "fireworks",
        "github-copilot",
        "google",
        "google-vertex",
        "groq",
        "huggingface",
        "kimi-coding",
        "minimax",
        "minimax-cn",
        "mistral",
        "moonshotai",
        "moonshotai-cn",
        "nvidia",
        "openai",
        "openai-codex",
        "opencode",
        "opencode-go",
        "openrouter",
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
        "qwen-token-plan-individual-cn",
        "radius",
        "together",
        "vercel-ai-gateway",
        "xai",
        "xiaomi",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-sgp",
        "zai",
        "zai-coding-cn",
    ];
    for id in expected {
        assert!(builtin_provider(id).is_some(), "missing provider {id}");
    }
}

#[test]
fn kimi_coding_definition_matches_ts() {
    let def = builtin_provider("kimi-coding").unwrap();
    assert_eq!(def.base_url, "https://api.kimi.com/coding/v1");
    assert_eq!(def.api, "openai-completions");
    // No "kimi" alias — ids match TS pi exactly.
    assert!(builtin_provider("kimi").is_none());
    // Moonshot-hosted: no cross-vendor ANTHROPIC_API_KEY fallback (that
    // would leak the user's Anthropic key to api.kimi.com).
    assert_eq!(def.env_keys, &["KIMI_API_KEY"]);
}

#[test]
fn azure_and_qwen_definitions_match_v1_6() {
    let azure = builtin_provider("azure").unwrap();
    assert_eq!(azure.name, "Azure");
    assert_eq!(azure.api, "azure-openai-responses");
    assert_eq!(azure.base_url, "");
    assert!(
        builtin_provider(LEGACY_AZURE_PROVIDER_ID).is_some_and(|legacy| {
            // The pre-Completions id remains a compatibility alias, not a
            // separately listed provider.
            std::ptr::eq(legacy, azure)
        })
    );

    let cases = [
        (
            "qwen-token-plan",
            "Qwen Token Plan Team",
            "https://token-plan.maas.qwencloudapi.com/compatible-mode/v1",
            &["QWEN_TOKEN_PLAN_TEAM_API_KEY", "QWEN_TOKEN_PLAN_API_KEY"][..],
        ),
        (
            "qwen-token-plan-cn",
            "Qwen Token Plan Team CN",
            "https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1",
            &[
                "QWEN_TOKEN_PLAN_TEAM_CN_API_KEY",
                "QWEN_TOKEN_PLAN_CN_API_KEY",
            ][..],
        ),
        (
            "qwen-token-plan-individual",
            "Qwen Token Plan Individual",
            "https://token-plan.maas.qwencloudapi.com/compatible-mode/v1",
            &[
                "QWEN_TOKEN_PLAN_INDIVIDUAL_API_KEY",
                "QWEN_TOKEN_PLAN_API_KEY",
            ][..],
        ),
        (
            "qwen-token-plan-individual-cn",
            "Qwen Token Plan Individual CN",
            "https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1",
            &[
                "QWEN_TOKEN_PLAN_INDIVIDUAL_CN_API_KEY",
                "QWEN_TOKEN_PLAN_CN_API_KEY",
            ][..],
        ),
    ];
    for (id, name, base_url, env_keys) in cases {
        let def = builtin_provider(id).unwrap();
        assert_eq!(def.name, name);
        assert_eq!(def.base_url, base_url);
        assert_eq!(def.api, "openai-completions");
        assert_eq!(def.env_keys, env_keys);
    }
}

#[test]
fn catalog_deserializes_full_models() {
    let models = builtin_models("anthropic");
    assert!(!models.is_empty());
    let opus = models.iter().find(|m| m.id.contains("opus")).unwrap();
    assert!(opus.context_window >= 200_000);
    assert!(opus.max_tokens > 0);
    assert!(opus.reasoning);
    assert_eq!(opus.api, "anthropic-messages");
    assert_eq!(opus.provider, "anthropic");
    assert!(opus.cost.input > 0.0);

    // thinkingLevelMap with null values must survive.
    let fable = models.iter().find(|m| m.id == "claude-fable-5").unwrap();
    let map = fable.thinking_level_map.as_ref().unwrap();
    assert!(map.contains_key("off"));
    assert!(map["off"].is_none());
}

#[test]
fn catalog_kimi_coding_has_k3() {
    let models = builtin_models("kimi-coding");
    let k3 = models.iter().find(|m| m.id == "k3").unwrap_or_else(|| {
        panic!(
            "models: {:?}",
            models.iter().map(|m| &m.id).collect::<Vec<_>>()
        )
    });
    assert_eq!(k3.api, "openai-completions");
    assert_eq!(k3.provider, "kimi-coding");
    assert_eq!(k3.base_url, "https://api.kimi.com/coding/v1");
    assert!(k3.reasoning);
    assert_eq!(
        k3.compat
            .as_ref()
            .and_then(|compat| compat.get("supportsReasoningEffort")),
        Some(&serde_json::json!(true))
    );
}

#[test]
fn catalog_azure_supports_responses_and_foundry_completions() {
    assert!(builtin_models("azure-openai-responses").is_empty());
    let models = builtin_models("azure");
    assert!(
        models
            .iter()
            .filter(|model| model.api == "azure-openai-responses")
            .all(|model| model.provider == "azure")
    );
    let deepseek = builtin_model("azure", "deepseek-v4-pro").unwrap();
    assert_eq!(deepseek.api, "openai-completions");
    assert_eq!(deepseek.base_url, "");
    assert_eq!(deepseek.cost.input, 1.925);
    assert_eq!(deepseek.cost.output, 3.828);
    assert_eq!(
        deepseek
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get("max")),
        Some(&None)
    );
    let compat = deepseek.compat.as_ref().unwrap();
    assert_eq!(
        compat.get("thinkingFormat"),
        Some(&serde_json::json!("openai"))
    );
    assert_eq!(
        compat.get("supportsLongCacheRetention"),
        Some(&serde_json::json!(false))
    );
}

#[test]
fn catalog_qwen_v1_6_lists_and_gateways() {
    let individual = [
        "deepseek-v4-flash-0731",
        "deepseek-v4-pro",
        "deepseek-v4-pro-0813",
        "deepseek-v4.1-flash",
        "deepseek-v4.1-flash-1017",
        "glm-5.2",
        "glm-5.3",
        "qwen3.6-flash",
        "qwen3.7-max",
        "qwen3.7-plus",
        "qwen3.8-flash",
        "qwen3.8-max",
        "qwen4-max-preview",
    ];
    for provider in [
        "qwen-token-plan-individual",
        "qwen-token-plan-individual-cn",
    ] {
        let models = builtin_models(provider);
        let mut ids = models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        let mut expected = individual;
        expected.sort_unstable();
        assert_eq!(ids, expected, "provider {provider}");
    }

    for provider in ["qwen-token-plan", "qwen-token-plan-cn"] {
        let models = builtin_models(provider);
        assert_eq!(models.len(), 20, "provider {provider}");
        for id in individual {
            assert!(models.iter().any(|model| model.id == id), "{provider}/{id}");
        }
    }
    for provider in [
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
        "qwen-token-plan-individual-cn",
    ] {
        let expected_host = if provider.ends_with("-cn") {
            "token-plan.maas.qianwenaiapi.com"
        } else {
            "token-plan.maas.qwencloudapi.com"
        };
        for model in builtin_models(provider).iter() {
            assert!(model.base_url.contains(expected_host), "{}", model.base_url);
            assert!(!model.base_url.contains("aliyuncs.com"));
            assert!(!matches!(
                model.id.as_str(),
                "deepseek-v3.2" | "deepseek-v4-flash" | "qwen3.8-max-preview"
            ));
        }
    }

    let qwen4 = builtin_model("qwen-token-plan-individual-cn", "qwen4-max-preview").unwrap();
    assert_eq!(qwen4.context_window, 1_000_000);
    assert_eq!(qwen4.max_tokens, 393_216);
    assert!(qwen4.input.contains(&tack_ai::types::InputKind::Image));
}

#[test]
fn catalog_copilot_completions_reasoning_metadata() {
    for id in [
        "gemini-4-argon",
        "gemini-4-carbon",
        "gemini-3.8-flash",
        "kimi-k3",
    ] {
        let model = builtin_model("github-copilot", id).unwrap();
        assert_eq!(model.api, "openai-completions");
        assert!(model.reasoning);
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.get("supportsReasoningEffort")),
            Some(&serde_json::json!(true)),
            "{id}"
        );
        assert!(model.thinking_level_map.is_some(), "{id}");
    }
    let argon = builtin_model("github-copilot", "gemini-4-argon").unwrap();
    let levels = argon.thinking_level_map.as_ref().unwrap();
    assert_eq!(levels.get("low"), Some(&Some("low".to_string())));
    assert_eq!(levels.get("high"), Some(&Some("high".to_string())));
    let carbon = builtin_model("github-copilot", "gemini-4-carbon").unwrap();
    assert_eq!(
        carbon
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get("medium")),
        Some(&Some("medium".to_string()))
    );
}

#[test]
fn default_model_picks_catalog_first() {
    let model = default_model_for("deepseek").unwrap();
    assert!(!model.id.is_empty());
    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.base_url, "https://api.deepseek.com");
}

#[test]
fn custom_providers_from_models_json() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("models.json"),
        r#"{
          "providers": {
            "ollama": {
              "baseUrl": "http://localhost:11434/v1",
              "api": "openai-completions",
              "apiKey": "ollama",
              "compat": { "supportsDeveloperRole": false },
              "models": [{ "id": "llama3.1:8b" }, { "id": "qwen3:32b", "reasoning": true, "contextWindow": 131072 }]
            }
          }
        }"#,
    )
    .unwrap();
    let custom = load_custom_providers(dir.path());
    assert_eq!(custom.len(), 1);
    assert_eq!(custom[0].id, "ollama");
    assert_eq!(custom[0].api_key.as_deref(), Some("ollama"));
    assert_eq!(custom[0].models.len(), 2);
    assert_eq!(custom[0].models[1].context_window, 131072);
    assert!(custom[0].models[1].reasoning);
    assert_eq!(custom[0].models[0].base_url, "http://localhost:11434/v1");
}

#[test]
fn env_var_interpolation_in_api_key() {
    let dir = tempfile::tempdir().unwrap();
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("TACK_TEST_KEY", "resolved-key");
    }
    std::fs::write(
        dir.path().join("models.json"),
        r#"{ "providers": { "x": { "baseUrl": "http://x", "api": "openai-completions", "apiKey": "$TACK_TEST_KEY", "models": [] } } }"#,
    )
    .unwrap();
    let custom = load_custom_providers(dir.path());
    assert_eq!(custom[0].api_key.as_deref(), Some("resolved-key"));
}

// ---------------------------------------------------------------------------
// Catalog override pinning (zai batch): the embedded catalog is replaced
// wholesale per provider by `install_catalog_override`; everything else is
// untouched. Uses a fictional provider id so global catalog state stays
// compatible with the other tests in this binary.
// ---------------------------------------------------------------------------

const PIN_PROVIDER: &str = "zzz-pin-override";

fn pin_catalog_json(model_id: &str) -> String {
    format!(
        r#"{{ "{PIN_PROVIDER}": {{ "api": "openai-completions", "models": [
            {{
                "id": "{model_id}",
                "name": "Pin Model",
                "api": "openai-completions",
                "provider": "{PIN_PROVIDER}",
                "baseUrl": "https://pin.example/v1",
                "reasoning": true,
                "thinkingLevelMap": {{ "off": null, "high": "max" }},
                "input": ["text"],
                "cost": {{ "input": 1.0, "output": 2.0, "cacheRead": 0.1, "cacheWrite": 0.2 }},
                "contextWindow": 123456,
                "maxTokens": 4096
            }}
        ] }} }}"#
    )
}

#[test]
fn catalog_override_replaces_wholesale_and_dedups() {
    // Unknown provider: empty, never panics.
    assert!(builtin_models(PIN_PROVIDER).is_empty());
    assert!(builtin_model(PIN_PROVIDER, "pin-a").is_none());

    let count_before = catalog_override_count();

    // Install: provider list appears, count increments by exactly one.
    let catalog = parse_catalog_json(&pin_catalog_json("pin-a")).unwrap();
    let prev = install_catalog_override(catalog);
    assert_eq!(prev, count_before);
    assert_eq!(catalog_override_count(), count_before + 1);

    let models = builtin_models(PIN_PROVIDER);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "pin-a");
    assert_eq!(models[0].context_window, 123456);
    // thinkingLevelMap with null values survives the override round-trip.
    let map = models[0].thinking_level_map.as_ref().unwrap();
    assert!(map.contains_key("off"));
    assert_eq!(map["off"], None);
    assert_eq!(map["high"], Some("max".to_string()));
    assert!(builtin_model(PIN_PROVIDER, "pin-a").is_some());

    // Re-install same provider: count unchanged (dedup), list replaced
    // wholesale — the old model is gone, not merged.
    let catalog = parse_catalog_json(&pin_catalog_json("pin-b")).unwrap();
    let prev = install_catalog_override(catalog);
    assert_eq!(prev, count_before + 1);
    assert_eq!(catalog_override_count(), count_before + 1);
    let models = builtin_models(PIN_PROVIDER);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "pin-b");
    assert!(builtin_model(PIN_PROVIDER, "pin-a").is_none());

    // Providers not in the override keep the embedded catalog.
    let anthropic = builtin_models("anthropic");
    assert!(!anthropic.is_empty());
    assert!(anthropic.iter().any(|m| m.id.contains("opus")));
}

#[test]
fn parse_catalog_json_rejects_garbage_and_bad_shape() {
    assert!(parse_catalog_json("not json").is_err());
    // Provider entry missing the required "api" field fails closed.
    assert!(parse_catalog_json(r#"{ "x": { "models": [] } }"#).is_err());
    // Empty document is a valid (no-op) catalog.
    assert!(parse_catalog_json("{}").unwrap().is_empty());
}

/// TS #9816/#9804: every openai-completions catalog model carries an
/// explicit `compat.supportsStrictMode` (runtime default is non-strict);
/// the whitelist semantics match upstream generate-models.
#[test]
fn catalog_openai_completions_models_have_explicit_strict_mode() {
    for def in BUILTIN_PROVIDERS {
        for m in builtin_models(def.id).iter() {
            if m.api != "openai-completions" {
                continue;
            }
            let strict = m
                .compat
                .as_ref()
                .and_then(|c| c.get("supportsStrictMode"))
                .and_then(|v| v.as_bool());
            assert!(
                strict.is_some(),
                "{}/{} missing explicit supportsStrictMode",
                def.id,
                m.id
            );
        }
    }

    // Spot-check the whitelist: capable providers opt in …
    let groq = builtin_model("groq", "openai/gpt-oss-20b").unwrap();
    assert_eq!(
        groq.compat
            .as_ref()
            .and_then(|c| c.get("supportsStrictMode")),
        Some(&serde_json::json!(true))
    );
    // … Cerebras is excluded (mixed strict/unstrict tools 400, TS #9804).
    for m in builtin_models("cerebras").iter() {
        assert_eq!(
            m.compat.as_ref().and_then(|c| c.get("supportsStrictMode")),
            Some(&serde_json::json!(false)),
            "cerebras/{} must be non-strict",
            m.id
        );
    }
}
