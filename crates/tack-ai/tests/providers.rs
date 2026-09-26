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
        "azure-openai-responses",
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
    assert_eq!(def.base_url, "https://api.kimi.com/coding");
    assert_eq!(def.api, "anthropic-messages");
    // No "kimi" alias — ids match TS pi exactly.
    assert!(builtin_provider("kimi").is_none());
    // Moonshot-hosted: no cross-vendor ANTHROPIC_API_KEY fallback (that
    // would leak the user's Anthropic key to api.kimi.com).
    assert_eq!(def.env_keys, &["KIMI_API_KEY"]);
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
    assert!(
        models.iter().any(|m| m.id == "k3"),
        "models: {:?}",
        models.iter().map(|m| &m.id).collect::<Vec<_>>()
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
