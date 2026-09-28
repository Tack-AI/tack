//! Developer tooling for tack-RPC v3 plugins (roadmap P2): the
//! `ext new` / `ext inspect` / `ext dev` / `ext test` subcommands.
//!
//! These commands speak v3 directly (they predate the ExtensionManager
//! switch): spawn the plugin's process carrier, run the initialize
//! handshake, and drive capability calls. `dev`/`test` share a JSON
//! scenario format:
//!
//! ```jsonc
//! {
//!   "initialize": { "trusted": true, "config": {} },   // all optional
//!   "steps": [
//!     { "call": "tools/execute", "params": {…}, "expect": {…} },
//!     { "call": "hooks/beforeToolCall", "params": {…}, "expectError": -32001 },
//!     { "notify": "events/lifecycle", "params": {…} },
//!     { "expectHostRequest": "ui/select", "respond": "b" },
//!     { "sleepMs": 50 }
//!   ]
//! }
//! ```
//!
//! `expect` is a recursive subset match (objects: every expected key is
//! present and matches; arrays: prefix-wise; scalars: equality).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tack_ext::rpc3::{
    ERR_CAPABILITY_NOT_GRANTED, ERR_METHOD_NOT_FOUND, ERR_POLICY_DENIED, ErrorObject,
    HostCapabilities, HostInfo, InitializeParams, RunMode, method,
};
use tack_ext::v3::{PeerError, PeerHandler, V3Process};

// ---------------------------------------------------------------------------
// Manifest (minimal: the process-carrier fields)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct DevManifest {
    name: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

fn read_manifest(dir: &Path) -> Result<DevManifest> {
    let path = dir.join("extension.json");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("bad manifest {}", path.display()))
}

/// Path-like args resolve against the extension directory (same rule as
/// the session loader).
fn resolve_args(dir: &Path, args: &[String]) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let path_like = arg.contains('/') || arg.contains('\\') || arg.starts_with('.');
            if path_like && !PathBuf::from(arg).is_absolute() {
                dir.join(arg).to_string_lossy().to_string()
            } else {
                arg.clone()
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// DevHost: plugin → host services with scenario scripting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ScriptedResponse {
    Result(Value),
    Error(i64, String),
}

#[derive(Debug)]
struct Expectation {
    method: String,
    response: ScriptedResponse,
}

/// The dev-host service surface. Requests matching the scenario's
/// `expectHostRequest` queue get the scripted answer; everything else
/// degrades deterministically (dialogs fail with a hint, `exec` is
/// trust-gated, notifications are printed).
struct DevHost {
    cwd: PathBuf,
    trusted: bool,
    config: Value,
    shell: Option<Arc<tack_tools::shell::ShellConfig>>,
    scripted: Mutex<VecDeque<Expectation>>,
}

impl std::fmt::Debug for DevHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevHost")
            .field("trusted", &self.trusted)
            .finish()
    }
}

impl DevHost {
    fn new(cwd: &Path, trusted: bool, config: Value) -> Self {
        let shell = tack_tools::shell::resolve_shell(None).ok().map(Arc::new);
        DevHost {
            cwd: cwd.to_path_buf(),
            trusted,
            config,
            shell,
            scripted: Mutex::new(VecDeque::new()),
        }
    }

    fn expect(&self, method: &str, response: ScriptedResponse) {
        self.scripted
            .lock()
            .expect("scripted queue")
            .push_back(Expectation {
                method: method.to_string(),
                response,
            });
    }

    fn error(code: i64, message: impl Into<String>) -> ErrorObject {
        ErrorObject {
            code,
            message: message.into(),
            data: None,
        }
    }

    async fn exec(&self, params: &Value) -> Result<Value, ErrorObject> {
        if !self.trusted {
            return Err(Self::error(
                ERR_POLICY_DENIED,
                "exec requires project trust",
            ));
        }
        let Some(shell) = &self.shell else {
            return Err(Self::error(
                ERR_CAPABILITY_NOT_GRANTED,
                "no shell available",
            ));
        };
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let timeout = std::time::Duration::from_millis(
            params
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(30_000),
        );
        let run = async {
            let output = tokio::process::Command::new(&shell.shell)
                .args(&shell.args)
                .arg(command)
                .current_dir(&self.cwd)
                .stdin(std::process::Stdio::null())
                .output()
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(serde_json::json!({
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
                "code": output.status.code().unwrap_or(-1),
            }))
        };
        match tokio::time::timeout(timeout, run).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(Self::error(ERR_CAPABILITY_NOT_GRANTED, e)),
            Err(_) => Err(Self::error(
                tack_ext::rpc3::ERR_REQUEST_TIMEOUT,
                "exec timed out",
            )),
        }
    }
}

#[async_trait::async_trait]
impl PeerHandler for DevHost {
    async fn handle_request(&self, rpc_method: &str, params: Value) -> Result<Value, ErrorObject> {
        // Scripted answers first (scenario expectHostRequest queue).
        {
            let mut queue = self.scripted.lock().expect("scripted queue");
            if queue.front().is_some_and(|e| e.method == rpc_method) {
                let expectation = queue.pop_front().expect("front checked");
                return match expectation.response {
                    ScriptedResponse::Result(value) => Ok(value),
                    ScriptedResponse::Error(code, message) => Err(Self::error(code, message)),
                };
            }
        }
        match rpc_method {
            method::UI_NOTIFY => {
                let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                eprintln!("[plugin notify] {message}");
                Ok(Value::Null)
            }
            method::UI_SELECT | method::UI_CONFIRM | method::UI_INPUT => Err(Self::error(
                ERR_CAPABILITY_NOT_GRANTED,
                format!(
                    "{rpc_method} is not scripted — add an expectHostRequest step to the scenario"
                ),
            )),
            method::EXEC_RUN => self.exec(&params).await,
            method::SESSION_GET => Ok(serde_json::json!({
                "sessionId": "dev",
                "mode": "print",
                "cwd": self.cwd,
                "trusted": self.trusted,
                "messageCount": 0,
            })),
            method::CONFIG_GET => Ok(serde_json::json!({ "config": self.config })),
            method::SHUTDOWN => Ok(Value::Null),
            other => Err(Self::error(
                ERR_METHOD_NOT_FOUND,
                format!("{other} is not available in the dev host"),
            )),
        }
    }

    async fn handle_notification(&self, rpc_method: &str, params: Value) {
        match rpc_method {
            method::LOGS_EMIT => {
                let level = params
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("info");
                let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                eprintln!("[plugin log:{level}] {message}");
            }
            method::WARNINGS_EMIT => {
                let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                eprintln!("[plugin warning] {message}");
            }
            method::WIDGETS_UPDATE => {
                let id = params.get("id").and_then(Value::as_str).unwrap_or("");
                eprintln!("[plugin widget] {id} updated");
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario format and runner
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Scenario {
    initialize: ScenarioInit,
    steps: Vec<Step>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ScenarioInit {
    trusted: Option<bool>,
    mode: Option<String>,
    config: Option<Value>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Step {
    call: Option<String>,
    params: Option<Value>,
    expect: Option<Value>,
    expect_error: Option<i64>,
    notify: Option<String>,
    expect_host_request: Option<String>,
    respond: Option<Value>,
    respond_error: Option<i64>,
    sleep_ms: Option<u64>,
}

/// Recursive subset match (see module docs).
fn is_subset(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|actual| is_subset(value, actual))
        }),
        (Value::Array(expected), Value::Array(actual)) => {
            expected.len() <= actual.len()
                && expected
                    .iter()
                    .zip(actual.iter())
                    .all(|(expected, actual)| is_subset(expected, actual))
        }
        (expected, actual) => expected == actual,
    }
}

struct StepOutcome {
    label: String,
    result: Result<(), String>,
}

fn run_mode(name: &str) -> Result<RunMode> {
    match name {
        "tui" => Ok(RunMode::Tui),
        "print" => Ok(RunMode::Print),
        "rpc" => Ok(RunMode::Rpc),
        "acp" => Ok(RunMode::Acp),
        other => bail!("unknown mode {other:?} (tui|print|rpc|acp)"),
    }
}

/// Run the scenario's steps against a live plugin (already
/// handshake-done). Returns one outcome per executable step.
async fn run_scenario(
    client: &tack_ext::v3::HostClient,
    host: &DevHost,
    scenario: &Scenario,
) -> Vec<StepOutcome> {
    let mut outcomes = Vec::new();
    for (index, step) in scenario.steps.iter().enumerate() {
        let label = format!(
            "step {}: {}",
            index + 1,
            step.call
                .as_deref()
                .or(step.notify.as_deref())
                .or(step.expect_host_request.as_deref())
                .unwrap_or("sleep")
        );
        let result = run_step(client, host, step).await;
        outcomes.push(StepOutcome { label, result });
    }
    outcomes
}

async fn run_step(
    client: &tack_ext::v3::HostClient,
    host: &DevHost,
    step: &Step,
) -> Result<(), String> {
    if let Some(method_name) = &step.expect_host_request {
        let response = match (step.respond.clone(), step.respond_error) {
            (Some(value), None) => ScriptedResponse::Result(value),
            (None, Some(code)) => ScriptedResponse::Error(code, "scripted error".to_string()),
            _ => ScriptedResponse::Result(Value::Null),
        };
        host.expect(method_name, response);
        return Ok(());
    }
    if let Some(ms) = step.sleep_ms {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        return Ok(());
    }
    if let Some(method_name) = &step.notify {
        return client
            .peer()
            .notify(method_name, step.params.clone().unwrap_or(Value::Null))
            .await
            .map_err(|e| e.to_string());
    }
    if let Some(method_name) = &step.call {
        let call = client
            .peer()
            .call(method_name, step.params.clone().unwrap_or(Value::Null));
        match (call.await, step.expect.clone(), step.expect_error) {
            (Ok(result), Some(expected), None) => {
                if is_subset(&expected, &result) {
                    Ok(())
                } else {
                    Err(format!(
                        "expectation mismatch\n  expected (subset): {expected}\n  actual: {result}"
                    ))
                }
            }
            (Ok(result), None, None) => {
                eprintln!("  result: {result}");
                Ok(())
            }
            (Err(PeerError::Remote(error)), _, Some(code)) if error.code == code => Ok(()),
            (Err(error), _, Some(code)) => Err(format!("expected error {code}, got {error}")),
            (Ok(result), _, Some(code)) => {
                Err(format!("expected error {code}, got result {result}"))
            }
            (Err(error), Some(expected), None) => Err(format!(
                "call failed: {error}\n  expected (subset): {expected}"
            )),
            (Err(error), None, None) => Err(format!("call failed: {error}")),
        }
    } else {
        Err("step has no action (call|notify|expectHostRequest|sleepMs)".to_string())
    }
}

// ---------------------------------------------------------------------------
// Shared spawn + handshake
// ---------------------------------------------------------------------------

async fn spawn_and_initialize(
    dir: &Path,
    scenario_init: &ScenarioInit,
) -> Result<(V3Process, Arc<DevHost>, tack_ext::rpc3::InitializeResult)> {
    let manifest = read_manifest(dir)?;
    let Some(command) = &manifest.command else {
        bail!(
            "{} declares no `command` (bundle-only extension) — nothing to run",
            manifest.name
        );
    };
    let args = resolve_args(dir, &manifest.args);
    let env: Vec<(String, String)> = manifest.env.clone().into_iter().collect();
    let trusted = scenario_init.trusted.unwrap_or_else(|| {
        crate::project_trust::is_trusted(dir, &tack_session::default_agent_dir())
    });
    if scenario_init.trusted == Some(true) {
        eprintln!("note: scenario overrides trust to trusted=true (exec is enabled)");
    }
    let config = scenario_init.config.clone().unwrap_or(Value::Null);
    let host = Arc::new(DevHost::new(dir, trusted, config.clone()));
    let handler: Arc<dyn PeerHandler> = host.clone();
    let process = V3Process::spawn(command, &args, &env, dir, handler)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let init = InitializeParams {
        protocol_version: tack_ext::v3::PROTOCOL_VERSION.to_string(),
        host: HostInfo {
            name: "tack-dev".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        mode: match &scenario_init.mode {
            Some(mode) => run_mode(mode)?,
            None => RunMode::Print,
        },
        cwd: dir.to_string_lossy().to_string(),
        trusted,
        capabilities: HostCapabilities {
            widgets: Some(true),
            autocomplete: Some(true),
            session_control: Some(true),
            snapshot: Some(false),
            ui_dialogs: Some(true),
            exec: Some(trusted),
            provider_registration: Some(false),
            metrics: None,
        },
        config: Some(config),
    };
    let result = process
        .client
        .initialize(&init)
        .await
        .map_err(|e| anyhow::anyhow!("handshake failed: {e}"))?;
    Ok((process, host, result))
}

// ---------------------------------------------------------------------------
// Subcommand entry points
// ---------------------------------------------------------------------------

/// `tack ext inspect <dir>`: handshake and dump the declared capabilities.
pub async fn cmd_ext_inspect(dir: &Path) -> Result<()> {
    let (mut process, _host, result) = spawn_and_initialize(dir, &ScenarioInit::default()).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    process.shutdown().await;
    Ok(())
}

/// `tack ext dev <dir> [scenario.json]`: handshake, print capabilities,
/// then run the scenario — or without one, stream plugin logs until
/// Ctrl-C (printf-debugging loop).
pub async fn cmd_ext_dev(dir: &Path, scenario_path: Option<&Path>) -> Result<()> {
    let scenario = match scenario_path {
        Some(path) => {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            serde_json::from_str::<Scenario>(&content)
                .with_context(|| format!("bad scenario {}", path.display()))?
        }
        None => Scenario::default(),
    };
    let (mut process, host, result) = spawn_and_initialize(dir, &scenario.initialize).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    if scenario_path.is_some() {
        let outcomes = run_scenario(&process.client, &host, &scenario).await;
        let mut failed = 0;
        for outcome in &outcomes {
            match &outcome.result {
                Ok(()) => println!("ok    {}", outcome.label),
                Err(e) => {
                    failed += 1;
                    println!("FAIL  {}: {e}", outcome.label);
                }
            }
        }
        process.shutdown().await;
        if failed > 0 {
            bail!("{failed} step(s) failed");
        }
        return Ok(());
    }
    println!("plugin running; Ctrl-C to stop (plugin logs stream to stderr)");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = process.peer.wait_dead() => {
            println!("plugin exited");
        }
    }
    process.shutdown().await;
    Ok(())
}

/// `tack ext test <dir> [scenario.json]`: scenario assertions with a
/// non-zero exit on failure (default: `<dir>/plugin.scenario.json`).
pub async fn cmd_ext_test(dir: &Path, scenario_path: Option<&Path>) -> Result<()> {
    let path = scenario_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dir.join("plugin.scenario.json"));
    let content = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "cannot read {} (no scenario given and no default found)",
            path.display()
        )
    })?;
    let scenario: Scenario = serde_json::from_str(&content)
        .with_context(|| format!("bad scenario {}", path.display()))?;
    let (mut process, host, _result) = spawn_and_initialize(dir, &scenario.initialize).await?;
    let outcomes = run_scenario(&process.client, &host, &scenario).await;
    let mut failed = 0;
    for outcome in &outcomes {
        match &outcome.result {
            Ok(()) => println!("ok    {}", outcome.label),
            Err(e) => {
                failed += 1;
                println!("FAIL  {}: {e}", outcome.label);
            }
        }
    }
    process.shutdown().await;
    if failed > 0 {
        bail!("{failed}/{} step(s) failed", outcomes.len());
    }
    println!("{} step(s) passed", outcomes.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// ext new: scaffolding
// ---------------------------------------------------------------------------

const RUST_MAIN: &str = r#"use serde_json::json;
use tack_ext_sdk::{Plugin, ToolSpec, text_output};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    Plugin::builder(env!("CARGO_PKG_NAME"))
        .version("0.1.0")
        .tool(
            ToolSpec {
                name: "hello.echo".to_string(),
                label: None,
                description: "Echo the arguments back".to_string(),
                parameters: json!({"type": "object"}),
            },
            |params, _cx| async move {
                Ok(text_output(format!("echo: {}", params.arguments)))
            },
        )
        .run()
        .await
}
"#;

const RUST_CARGO: &str = r#"[package]
name = "PLUGIN_NAME"
version = "0.1.0"
edition = "2021"

[dependencies]
# tack-ext-sdk is not yet published — point this at your tack checkout.
tack-ext-sdk = { path = "/path/to/tack/crates/tack-ext-sdk" }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
"#;

const TS_PLUGIN: &str = r#"import { plugin, textOutput } from "@tack/plugin";

plugin({ name: "PLUGIN_NAME", version: "0.1.0" })
  .tool(
    {
      name: "hello.echo",
      description: "Echo the arguments back",
      parameters: { type: "object" },
    },
    async (params) => textOutput(`echo: ${JSON.stringify(params.arguments)}`),
  )
  .run();
"#;

const TS_PACKAGE: &str = r#"{
  "name": "PLUGIN_NAME",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "dependencies": {
    "@tack/plugin": "file:/path/to/tack/sdk/typescript"
  }
}
"#;

const PY_PLUGIN: &str = r#""""A tack-RPC v3 plugin. Install the SDK first:
    pip install /path/to/tack/sdk/python
"""

from tack_plugin import Plugin, text_output

plugin = Plugin("PLUGIN_NAME", version="0.1.0").tool(
    {"name": "hello.echo", "description": "Echo the arguments back",
     "parameters": {"type": "object"}},
    lambda params, cx: text_output(f"echo: {params['arguments']}"),
)

plugin.run()
"#;

const SCENARIO: &str = r#"{
  "steps": [
    {
      "call": "tools/execute",
      "params": { "name": "hello.echo", "toolCallId": "c-1", "arguments": {"text": "hi"} },
      "expect": { "content": [ { "type": "text" } ] }
    }
  ]
}
"#;

const NEW_README: &str = r#"# PLUGIN_NAME

A tack-RPC v3 plugin. Develop it with:

```sh
tack ext inspect .     # handshake + dump declared capabilities
tack ext test .        # run plugin.scenario.json assertions
tack ext dev .         # run and stream plugin logs (Ctrl-C to stop)
```

Then install it with `tack ext install .` (note: the session loader
still speaks the v1/v2 protocol until the loader switch lands — use the
dev tooling above for v3 development).
"#;

/// `tack ext new <dir> <rust|ts|python>`: scaffold a plugin.
pub fn cmd_ext_new(dir: &Path, lang: &str) -> Result<()> {
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("plugin")
        .to_string();
    let fill = |template: &str| template.replace("PLUGIN_NAME", &name);
    let (manifest, sources, readme): (String, Vec<(&str, String)>, String) = match lang {
        "rust" => (
            serde_json::json!({
                "name": name,
                "command": "cargo",
                "args": ["run", "--quiet", "--manifest-path", "./Cargo.toml"]
            })
            .to_string(),
            vec![
                ("Cargo.toml", fill(RUST_CARGO)),
                ("src/main.rs", fill(RUST_MAIN)),
            ],
            fill(NEW_README),
        ),
        "ts" => (
            serde_json::json!({
                "name": name,
                "command": "node",
                "args": ["plugin.js"]
            })
            .to_string(),
            vec![
                ("package.json", fill(TS_PACKAGE)),
                ("plugin.js", fill(TS_PLUGIN)),
            ],
            fill(NEW_README),
        ),
        "python" => (
            serde_json::json!({
                "name": name,
                "command": "python3",
                "args": ["plugin.py"]
            })
            .to_string(),
            vec![("plugin.py", fill(PY_PLUGIN))],
            fill(NEW_README),
        ),
        other => bail!("unknown language {other:?} (rust|ts|python)"),
    };
    std::fs::create_dir_all(dir)?;
    for (relative, content) in sources {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
    }
    std::fs::write(dir.join("extension.json"), manifest)?;
    std::fs::write(dir.join("plugin.scenario.json"), SCENARIO)?;
    std::fs::write(dir.join("README.md"), readme)?;
    println!("scaffolded a {lang} plugin in {}", dir.display());
    println!("next: tack ext inspect {}", dir.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn subset_matching() {
        assert!(is_subset(
            &serde_json::json!({"a": 1}),
            &serde_json::json!({"a": 1, "b": 2})
        ));
        assert!(is_subset(
            &serde_json::json!({"content": [{"type": "text"}]}),
            &serde_json::json!({"content": [{"type": "text", "text": "hi"}], "extra": true})
        ));
        assert!(!is_subset(
            &serde_json::json!({"a": 2}),
            &serde_json::json!({"a": 1})
        ));
        assert!(!is_subset(
            &serde_json::json!([1, 2, 3]),
            &serde_json::json!([1, 2])
        ));
        assert!(is_subset(
            &serde_json::json!(null),
            &serde_json::json!(null)
        ));
    }

    #[test]
    fn scenario_parses() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
              "initialize": {"trusted": true},
              "steps": [
                {"call": "tools/execute", "params": {}, "expect": {}},
                {"call": "hooks/beforeToolCall", "params": {}, "expectError": -32001},
                {"expectHostRequest": "ui/select", "respond": "b"},
                {"sleepMs": 10}
              ]
            }"#,
        )
        .unwrap();
        assert_eq!(scenario.initialize.trusted, Some(true));
        assert_eq!(scenario.steps.len(), 4);
        assert_eq!(scenario.steps[1].expect_error, Some(-32001));
    }

    /// Path of the built demo-plugin bin (fixture), derived from the
    /// test binary location (…/target/<profile>/deps/…).
    fn demo_plugin_bin() -> Option<PathBuf> {
        let mut path = std::env::current_exe().ok()?;
        path.pop(); // deps/
        path.pop(); // profile/
        path.push(format!(
            "tack-v3-demo-plugin{}",
            std::env::consts::EXE_SUFFIX
        ));
        path.is_file().then_some(path)
    }

    /// Full dev-tool loop against the real fixture plugin: handshake,
    /// tool call with subset expectation, verdicts, scripted error.
    #[tokio::test]
    async fn scenario_e2e_against_demo_plugin() {
        let Some(bin) = demo_plugin_bin() else {
            eprintln!("demo plugin bin not built; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({
            "name": "demo",
            "command": bin,
            "args": []
        });
        std::fs::write(dir.path().join("extension.json"), manifest.to_string()).unwrap();
        let scenario: Scenario = serde_json::from_str(
            r#"{
              "initialize": {"trusted": false},
              "steps": [
                {"call": "tools/execute",
                 "params": {"name": "hello.echo", "toolCallId": "c-1", "arguments": {"x": 1}},
                 "expect": {"content": [{"type": "text", "text": "echo: {\"x\":1}"}]}},
                {"call": "hooks/beforeToolCall",
                 "params": {"toolCall": {"toolCallId": "c-2", "toolName": "bash",
                            "arguments": {"command": "rm -rf /"}}},
                 "expect": {"action": "deny"}},
                {"call": "hooks/beforeToolCall",
                 "params": {"toolCall": {"toolCallId": "c-3", "toolName": "read", "arguments": {}}},
                 "expect": {"action": "allow"}}
              ]
            }"#,
        )
        .unwrap();
        let (mut process, host, result) = spawn_and_initialize(dir.path(), &scenario.initialize)
            .await
            .unwrap();
        assert_eq!(result.plugin.name, "tack-v3-demo");
        let outcomes = run_scenario(&process.client, &host, &scenario).await;
        process.shutdown().await;
        for outcome in &outcomes {
            assert!(
                outcome.result.is_ok(),
                "{}: {:?}",
                outcome.label,
                outcome.result
            );
        }
        assert_eq!(outcomes.len(), 3);
    }

    /// Scripted plugin→host answers: the expectHostRequest queue answers
    /// a ui/select from inside a tool handler.
    #[tokio::test]
    async fn scripted_host_request_flow() {
        let Some(bin) = demo_plugin_bin() else {
            eprintln!("demo plugin bin not built; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({
            "name": "demo",
            "command": bin,
            "args": []
        });
        std::fs::write(dir.path().join("extension.json"), manifest.to_string()).unwrap();
        let (mut process, host, _result) =
            spawn_and_initialize(dir.path(), &ScenarioInit::default())
                .await
                .unwrap();
        // The demo plugin's hello.select tool asks ui/select; script "b".
        host.expect(
            "ui/select",
            ScriptedResponse::Result(serde_json::json!("b")),
        );
        let output = process
            .client
            .tool_execute(&tack_ext::rpc3::ToolExecuteParams {
                name: "hello.select".to_string(),
                tool_call_id: "c-1".to_string(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert_eq!(output.content[0].text.as_deref(), Some("picked: b"));
        process.shutdown().await;
    }

    #[test]
    fn ext_new_scaffolds_parseable_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("my-plugin");
        cmd_ext_new(&target, "python").unwrap();
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(target.join("extension.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["name"], "my-plugin");
        assert_eq!(manifest["command"], "python3");
        assert!(target.join("plugin.py").is_file());
        assert!(target.join("plugin.scenario.json").is_file());
        // Unknown language refuses before writing anything.
        let other = dir.path().join("bad");
        assert!(cmd_ext_new(&other, "cobol").is_err());
    }
}
