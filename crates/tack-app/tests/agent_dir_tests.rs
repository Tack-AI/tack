//! Process-level coverage for the `--agent-dir` CLI override.

#[test]
fn agent_dir_loads_custom_providers_without_the_environment_variable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_dir = temp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("models.json"),
        r#"{
          "providers": {
            "mock": {
              "baseUrl": "http://127.0.0.1:1/v1",
              "api": "openai-completions",
              "apiKey": "test-key",
              "models": [{"id": "mock-model", "contextWindow": 128000}]
            }
          }
        }"#,
    )
    .expect("custom provider catalog");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tack"))
        .arg("--agent-dir")
        .arg(&agent_dir)
        .args(["models", "mock-model"])
        .env("HOME", temp.path().join("home"))
        .env("TACK_OFFLINE", "1")
        .env_remove("TACK_AGENT_DIR")
        .output()
        .expect("run tack models");

    assert!(
        output.status.success(),
        "tack models failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mock-model"), "stdout: {stdout}");
    assert!(stdout.contains("mock"), "stdout: {stdout}");
}
