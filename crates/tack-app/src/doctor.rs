//! `tack doctor`: one-shot environment self-check. Every external
//! dependency the agent can use is probed and reported with a fix hint —
//! this is the first thing to ask for on any bug report.

use std::path::{Path, PathBuf};

use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn icon(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
    /// Fix hint shown on Warn/Fail.
    hint: Option<String>,
}

fn ok(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Ok,
        detail: detail.into(),
        hint: None,
    }
}

fn warn(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Warn,
        detail: detail.into(),
        hint: Some(hint.into()),
    }
}

fn fail(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Fail,
        detail: detail.into(),
        hint: Some(hint.into()),
    }
}

/// Command resolvable on PATH (or an absolute path that exists)?
fn command_exists(command: &str) -> Option<String> {
    let path = Path::new(command);
    if path.is_absolute() {
        return path.exists().then(|| command.to_string());
    }
    let probe = if cfg!(windows) { "where" } else { "which" };
    let output = std::process::Command::new(probe)
        .arg(command)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

fn check_shell(settings: &crate::settings::Settings) -> Check {
    match tack_tools::shell::resolve_shell(settings.shell_path.as_deref()) {
        Ok(config) => ok("shell", format!("{}", config.shell.display())),
        Err(_) => fail(
            "shell",
            "no bash found",
            "install Git for Windows / add bash to PATH / set shellPath",
        ),
    }
}

fn check_command(name: &'static str, command: &str, hint: &str, required: bool) -> Check {
    match command_exists(command) {
        Some(path) => ok(name, path),
        None if required => fail(name, format!("`{command}` not found"), hint),
        None => warn(name, format!("`{command}` not found"), hint),
    }
}

fn check_lsp_servers() -> Vec<Check> {
    let defaults: &[(&str, &str)] = &[
        ("rust-analyzer", "rust-analyzer"),
        ("typescript-language-server", "typescript-language-server"),
        ("pyright", "pyright-langserver"),
        ("gopls", "gopls"),
        ("clangd", "clangd"),
    ];
    defaults
        .iter()
        .map(|(name, command)| {
            check_command(
                Box::leak(format!("lsp: {name}").into_boxed_str()),
                command,
                "install it for LSP navigation/diagnostics, or silence this via lspServers/features.lsp",
                false,
            )
        })
        .collect()
}

fn check_sandbox() -> Check {
    match tack_tools::sandbox::detect() {
        Some(tack_tools::sandbox::SandboxBackend::Bubblewrap(p)) => {
            ok("sandbox backend", format!("bubblewrap ({})", p.display()))
        }
        Some(tack_tools::sandbox::SandboxBackend::Seatbelt(p)) => {
            ok("sandbox backend", format!("seatbelt ({})", p.display()))
        }
        Some(tack_tools::sandbox::SandboxBackend::WindowsJob) => ok(
            "sandbox backend",
            "Windows Job Objects (resource containment; no fs isolation)",
        ),
        None => warn(
            "sandbox backend",
            "none available",
            "Linux: install bubblewrap; macOS: sandbox-exec is built in",
        ),
    }
}

fn check_managed_tool(name: &'static str, candidates: &[&str]) -> Check {
    // Managed bin dir first (downloaded by tack), then PATH.
    let bin = tack_tools::shell::managed_bin_dir();
    for candidate in candidates {
        let managed = bin.join(if cfg!(windows) {
            format!("{candidate}.exe")
        } else {
            candidate.to_string()
        });
        if managed.exists() {
            return ok(name, format!("{} (managed)", managed.display()));
        }
        if let Some(path) = command_exists(candidate) {
            return ok(name, path);
        }
    }
    warn(
        name,
        format!("not found (managed dir: {})", bin.display()),
        "start the TUI once to auto-download, or install manually",
    )
}

fn check_browser() -> Check {
    match tack_tools::browser::find_browser() {
        Some(path) => ok("headless browser", format!("{}", path.display())),
        None => warn(
            "headless browser",
            "no Chrome/Edge/Chromium found",
            "install Chrome or Edge for JS-heavy web_fetch rendering (or set TACK_BROWSER)",
        ),
    }
}

fn check_credentials(agent_dir: &Path) -> Check {
    let auth_file = agent_dir.join("auth.json");
    let mut providers: Vec<String> = Vec::new();
    if let Ok(content) = std::fs::read_to_string(&auth_file)
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(map) = json.as_object()
    {
        providers.extend(map.keys().cloned());
    }
    for env in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY"] {
        if std::env::var_os(env).is_some() {
            providers.push(format!("env:{env}"));
        }
    }
    if providers.is_empty() {
        fail(
            "credentials",
            "no auth.json entries, no provider API keys in env",
            "run `tack login --provider <p>` or set an API key env var",
        )
    } else {
        ok("credentials", providers.join(", "))
    }
}

fn check_mcp(cwd: &Path, agent_dir: &Path) -> Check {
    let specs = crate::mcp_config::configured_servers(cwd, agent_dir);
    if specs.is_empty() {
        return ok("mcp servers", "none configured");
    }
    let mut problems = Vec::new();
    for spec in &specs {
        if let tack_tools::mcp::McpTransport::Stdio { command, .. } = &spec.transport
            && command_exists(command).is_none()
        {
            problems.push(format!("{}: `{command}` not on PATH", spec.name));
        }
    }
    if problems.is_empty() {
        ok(
            "mcp servers",
            format!("{} configured, all commands found", specs.len()),
        )
    } else {
        warn(
            "mcp servers",
            problems.join("; "),
            "fix the command paths in mcp.json",
        )
    }
}

fn check_dirs(agent_dir: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    match std::fs::create_dir_all(agent_dir.join("sessions")).map(|_| ()) {
        Ok(()) => ok("agent dir", format!("{} (writable)", agent_dir.display())),
        Err(e) => fail(
            "agent dir",
            format!("{}: {e}", agent_dir.display()),
            "check permissions",
        ),
    };
    for (file, name) in [
        (agent_dir.join("settings.json"), "settings.json"),
        (agent_dir.join("cron.json"), "cron.json"),
        (agent_dir.join("permissions.json"), "permissions.json"),
    ] {
        if file.exists() {
            match std::fs::read_to_string(&file)
                .ok()
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            {
                Some(_) => checks.push(ok(Box::leak(name.to_string().into_boxed_str()), "parses")),
                None => checks.push(fail(
                    Box::leak(name.to_string().into_boxed_str()),
                    format!("{} is malformed JSON", file.display()),
                    "fix or delete the file",
                )),
            }
        }
    }
    checks
}

/// Everything a report run needs, gathered once (text / JSON / bundle all
/// render from this).
struct Report {
    checks: Vec<Check>,
    cwd: PathBuf,
    agent_dir: PathBuf,
}

async fn collect_report() -> Result<Report> {
    let cwd = std::env::current_dir()?;
    let agent_dir = tack_session::default_agent_dir();
    let settings = crate::settings::Settings::load(&cwd, &agent_dir);

    let mut checks: Vec<Check> = vec![
        check_shell(&settings),
        check_command(
            "git",
            "git",
            "install git (needed for worktree isolation and checkpoint baselines)",
            true,
        ),
        check_sandbox(),
        check_browser(),
        check_credentials(&agent_dir),
        check_mcp(&cwd, &agent_dir),
        check_managed_tool("fd", &["fd", "fdfind"]),
        check_managed_tool("rg", &["rg"]),
    ];
    checks.extend(check_lsp_servers());
    checks.extend(check_dirs(&agent_dir));
    Ok(Report {
        checks,
        cwd,
        agent_dir,
    })
}

fn worst_status(checks: &[Check]) -> Status {
    let mut worst = Status::Ok;
    for check in checks {
        worst = match (worst, check.status) {
            (Status::Fail, _) | (_, Status::Fail) => Status::Fail,
            (Status::Warn, _) | (_, Status::Warn) => Status::Warn,
            _ => Status::Ok,
        };
    }
    worst
}

fn exit_code(status: Status) -> i32 {
    if status == Status::Fail { 1 } else { 0 }
}

fn render_text(report: &Report) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("tack doctor\n\n");
    for check in &report.checks {
        let _ = writeln!(
            out,
            "{} {:<24} {}",
            check.status.icon(),
            check.name,
            check.detail
        );
        if let Some(hint) = &check.hint {
            let _ = writeln!(out, "    → {hint}");
        }
    }
    out.push('\n');
    match worst_status(&report.checks) {
        Status::Ok => out.push_str("all good.\n"),
        Status::Warn => out.push_str("warnings only — tack works, some features are degraded.\n"),
        Status::Fail => out.push_str("problems found — fix the ✗ items above.\n"),
    }
    out
}

/// Machine-readable report: version/platform metadata + every check. This is
/// the shape bug-report tooling (and `--bundle`'s doctor.json) consumes.
fn report_json(report: &Report) -> serde_json::Value {
    let summary = worst_status(&report.checks);
    let env: serde_json::Map<String, serde_json::Value> = ["TERM", "COLORTERM", "SHELL", "CI"]
        .iter()
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .map(|v| (k.to_string(), serde_json::Value::String(v)))
        })
        .collect();
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cwd": report.cwd,
        "agentDir": report.agent_dir,
        "env": env,
        "summary": summary.as_str(),
        "checks": report
            .checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "status": c.status.as_str(),
                    "detail": c.detail,
                    "hint": c.hint,
                })
            })
            .collect::<Vec<_>>(),
    })
}

/// Run all checks and print the report. Returns the process exit code
/// (1 when any check fails hard).
pub async fn run() -> Result<i32> {
    let report = collect_report().await?;
    print!("{}", render_text(&report));
    Ok(exit_code(worst_status(&report.checks)))
}

/// JSON variant of `run` (for scripts and the bug-report template).
pub async fn run_json() -> Result<i32> {
    let report = collect_report().await?;
    println!("{}", serde_json::to_string_pretty(&report_json(&report))?);
    Ok(exit_code(worst_status(&report.checks)))
}

fn looks_sensitive(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("token")
        || k.contains("secret")
        || k.contains("password")
        || k.contains("credential")
        || k.contains("apikey")
        || k.contains("api_key")
        || k == "key"
        || k.ends_with("_key")
        || k.ends_with("-key")
}

/// Recursively replace values whose key looks like a credential with `***`,
/// so a bundled settings.json is safe to attach to a public issue.
fn redact_secrets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if looks_sensitive(k) {
                    *v = serde_json::Value::String("***".into());
                } else {
                    redact_secrets(v);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_secrets),
        _ => {}
    }
}

fn add_to_bundle<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, name, data)?;
    Ok(())
}

/// Write a bug-report bundle (tar.gz): the doctor report in text + JSON, a
/// redacted copy of settings.json, and crash.log if present. Credentials
/// (auth.json) are never included.
pub async fn run_bundle(path: Option<PathBuf>) -> Result<i32> {
    let report = collect_report().await?;
    let status = worst_status(&report.checks);
    let out = path.unwrap_or_else(|| {
        PathBuf::from(format!(
            "tack-doctor-{}.tar.gz",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        ))
    });

    let file = std::fs::File::create(&out)?;
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(gz);

    add_to_bundle(&mut builder, "doctor.txt", render_text(&report).as_bytes())?;
    add_to_bundle(
        &mut builder,
        "doctor.json",
        serde_json::to_string_pretty(&report_json(&report))?.as_bytes(),
    )?;
    if let Ok(content) = std::fs::read_to_string(report.agent_dir.join("settings.json"))
        && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&content)
    {
        redact_secrets(&mut value);
        add_to_bundle(
            &mut builder,
            "settings-redacted.json",
            serde_json::to_string_pretty(&value)?.as_bytes(),
        )?;
    }
    if let Ok(bytes) = std::fs::read(report.agent_dir.join("crash.log")) {
        add_to_bundle(&mut builder, "crash.log", &bytes)?;
    }
    builder.into_inner()?.finish()?;

    println!("wrote {}", out.display());
    println!("review the contents before attaching — paths and usernames are included as-is.");
    Ok(exit_code(status))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn sensitive_keys_are_detected() {
        for key in [
            "apiKey",
            "api_key",
            "ANTHROPIC_API_KEY",
            "token",
            "access_token",
            "client-secret",
            "password",
            "credentials",
            "key",
            "ssh_key",
        ] {
            assert!(looks_sensitive(key), "{key} should be sensitive");
        }
        for key in ["model", "updateRepo", "defaultTools", "monkey", "keyboard"] {
            assert!(!looks_sensitive(key), "{key} should not be sensitive");
        }
    }

    #[test]
    fn redaction_is_recursive() {
        let mut value = serde_json::json!({
            "model": "claude",
            "providers": [{"name": "x", "apiKey": "sk-real", "extra": {"token": 1}}],
            "oauth": {"access_token": "tok", "nested": {"password": "pw"}},
        });
        redact_secrets(&mut value);
        assert_eq!(value["model"], "claude");
        assert_eq!(value["providers"][0]["apiKey"], "***");
        assert_eq!(value["providers"][0]["extra"]["token"], "***");
        assert_eq!(value["oauth"]["access_token"], "***");
        assert_eq!(value["oauth"]["nested"]["password"], "***");
    }

    #[test]
    fn json_report_has_metadata_and_checks() {
        let report = Report {
            checks: vec![
                ok("shell", "/bin/bash"),
                warn("rg", "not found", "install ripgrep"),
            ],
            cwd: PathBuf::from("/tmp/x"),
            agent_dir: PathBuf::from("/tmp/agent"),
        };
        let json = report_json(&report);
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["summary"], "warn");
        assert_eq!(json["checks"].as_array().unwrap().len(), 2);
        assert_eq!(json["checks"][1]["hint"], "install ripgrep");
        assert_eq!(worst_status(&report.checks), Status::Warn);
        assert_eq!(exit_code(Status::Warn), 0);
        assert_eq!(exit_code(Status::Fail), 1);
    }

    #[test]
    fn bundle_roundtrip_lists_files() {
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            add_to_bundle(&mut builder, "doctor.txt", b"hello").unwrap();
            builder.finish().unwrap();
        }
        let mut archive = tar::Archive::new(&buf[..]);
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| String::from_utf8_lossy(&e.unwrap().path_bytes()).into_owned())
            .collect();
        assert_eq!(names, ["doctor.txt"]);
    }
}
