//! registerProvider: plugin-registered runtime providers resolve like
//! models.json customs.
#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]

use tack_ai::providers::{CustomModel, RuntimeProviderSpec};

fn spec(id: &str) -> RuntimeProviderSpec {
    RuntimeProviderSpec {
        id: id.to_string(),
        base_url: "http://localhost:9999".to_string(),
        api: "openai-completions".to_string(),
        api_key: None,
        api_key_env: None,
        headers: None,
        compat: None,
        models: vec![CustomModel {
            id: "test-model-1".to_string(),
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

#[test]
fn runtime_provider_resolves_as_model() {
    let id = "test-rt-provider";
    tack_ai::providers::register_runtime_provider(spec(id)).unwrap();

    let agent_dir = tempfile::tempdir().unwrap();
    let model = tack_app::model::resolve_model(id, Some("test-model-1"), agent_dir.path()).unwrap();
    assert_eq!(model.provider, id);
    assert_eq!(model.api, "openai-completions");
    assert_eq!(model.base_url, "http://localhost:9999");
    assert_eq!(model.context_window, 128_000);

    // Default model = first registered.
    let default = tack_app::model::resolve_model(id, None, agent_dir.path()).unwrap();
    assert_eq!(default.id, "test-model-1");

    // Unknown model id errors with a useful message.
    assert!(tack_app::model::resolve_model(id, Some("nope"), agent_dir.path()).is_err());
}

#[test]
fn runtime_provider_api_key_from_env() {
    let id = "test-rt-key";
    let mut spec = spec(id);
    spec.api_key_env = Some("TACK_TEST_RT_KEY".to_string());
    unsafe { std::env::set_var("TACK_TEST_RT_KEY", "sk-runtime") };
    tack_ai::providers::register_runtime_provider(spec).unwrap();

    let agent_dir = tempfile::tempdir().unwrap();
    let key = tack_app::model::resolve_api_key(id, None, agent_dir.path());
    assert_eq!(key.as_deref(), Some("sk-runtime"));
}

#[test]
fn invalid_spec_rejected() {
    let mut bad = spec("bad");
    bad.models.clear();
    assert!(tack_ai::providers::register_runtime_provider(bad).is_err());
}
