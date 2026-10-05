//! `tack mcp` management subcommands (add/remove/list/login/logout).
//! These run WITHOUT a session — no extensions are loaded, mirroring
//! `pi mcp` shell commands. File edits preserve unrelated mcp.json
//! content (only the named entry is touched).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

/// `add` arguments (built by the clap layer in main.rs).
#[derive(Debug)]
pub struct AddArgs {
    pub name: String,
    pub local: bool,
    pub url: Option<String>,
    pub transport: Option<String>,
    pub env: Vec<String>,
    pub headers: Vec<String>,
    pub bearer_token_env_var: Option<String>,
    pub command: Vec<String>,
}

pub fn user_mcp_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("mcp.json")
}

pub fn project_mcp_path(cwd: &Path) -> PathBuf {
    cwd.join(".pi").join("mcp.json")
}

/// Server names may contain only letters, digits, `_`, and `-` (the rule
/// other MCP clients share; tools are named `mcp__<server>__<tool>`).
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("server name {name:?} is invalid: use only letters, digits, '_' and '-'");
    }
    Ok(())
}

/// Parse repeated `KEY=VALUE` CLI flags into a JSON object.
fn kv_object(pairs: &[String], flag: &str) -> Result<serde_json::Map<String, Value>> {
    let mut out = serde_json::Map::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            bail!("--{flag} expects KEY=VALUE, got {pair:?}");
        };
        if key.is_empty() {
            bail!("--{flag} expects a non-empty KEY, got {pair:?}");
        }
        out.insert(key.to_string(), Value::String(value.to_string()));
    }
    Ok(out)
}

/// Read a JSON config file as an object; a missing file is an empty
/// object. A MALFORMED file is an error — `add`/`remove` must never
/// clobber content they could not parse.
fn load_json_object(path: &Path) -> Result<serde_json::Map<String, Value>> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(serde_json::Map::new());
    };
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    value
        .as_object()
        .cloned()
        .with_context(|| format!("{} must contain a JSON object", path.display()))
}

fn save_json_object(path: &Path, map: &serde_json::Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(&Value::Object(map.clone()))?;
    crate::atomic_write::atomic_write_private(path, &content, 0o600)?;
    Ok(())
}

/// Insert/replace one entry under `mcpServers` (other top-level keys and
/// other servers are preserved).
fn upsert_server(path: &Path, name: &str, entry: Value) -> Result<()> {
    let mut doc = load_json_object(path)?;
    doc.entry("mcpServers")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .with_context(|| format!("{}: \"mcpServers\" is not an object", path.display()))?
        .insert(name.to_string(), entry);
    save_json_object(path, &doc)
}

/// Remove one entry; false when it was not there.
fn remove_server(path: &Path, name: &str) -> Result<bool> {
    let mut doc = load_json_object(path)?;
    let Some(servers) = doc.get_mut("mcpServers").and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    if servers.remove(name).is_none() {
        return Ok(false);
    }
    save_json_object(path, &doc)?;
    Ok(true)
}

/// `tack mcp add`: write a stdio (`-- command args…`) or remote (`--url`)
/// entry to the user-level or project (`--local`) mcp.json.
pub fn add(args: &AddArgs, cwd: &Path, agent_dir: &Path) -> Result<()> {
    validate_name(&args.name)?;
    let path = if args.local {
        project_mcp_path(cwd)
    } else {
        user_mcp_path(agent_dir)
    };
    let entry = match (&args.url, args.command.first()) {
        (Some(_), Some(_)) => bail!("give either --url or a command after `--`, not both"),
        (None, None) => bail!("give a server URL (--url) or a stdio command after `--`"),
        (Some(url), None) => {
            let mut entry = serde_json::Map::new();
            if let Some(transport) = &args.transport {
                entry.insert("type".to_string(), Value::String(transport.clone()));
            }
            entry.insert("url".to_string(), Value::String(url.clone()));
            let mut headers = kv_object(&args.headers, "header")?;
            if let Some(var) = &args.bearer_token_env_var {
                headers.insert(
                    "Authorization".to_string(),
                    Value::String(format!("Bearer ${{{var}}}")),
                );
            }
            if !headers.is_empty() {
                entry.insert("headers".to_string(), Value::Object(headers));
            }
            Value::Object(entry)
        }
        (None, Some(command)) => {
            let mut entry = serde_json::Map::new();
            entry.insert("command".to_string(), Value::String(command.clone()));
            let rest: Vec<Value> = args.command[1..]
                .iter()
                .map(|a| Value::String(a.clone()))
                .collect();
            if !rest.is_empty() {
                entry.insert("args".to_string(), Value::Array(rest));
            }
            let env = kv_object(&args.env, "env")?;
            if !env.is_empty() {
                entry.insert("env".to_string(), Value::Object(env));
            }
            Value::Object(entry)
        }
    };
    upsert_server(&path, &args.name, entry)?;
    println!(
        "Added MCP server {:?} to {} ({}).",
        args.name,
        path.display(),
        if args.local { "project" } else { "user" }
    );
    Ok(())
}

/// `tack mcp remove`: delete an entry from the user-level (default) or
/// project (`--local`) file.
pub fn remove(name: &str, local: bool, cwd: &Path, agent_dir: &Path) -> Result<()> {
    let path = if local {
        project_mcp_path(cwd)
    } else {
        user_mcp_path(agent_dir)
    };
    if remove_server(&path, name)? {
        println!("Removed MCP server {name:?} from {}.", path.display());
        return Ok(());
    }
    // Point at the other file when the entry lives there.
    let other = if local {
        user_mcp_path(agent_dir)
    } else {
        project_mcp_path(cwd)
    };
    let other_doc = load_json_object(&other)?;
    let elsewhere = other_doc
        .get("mcpServers")
        .and_then(Value::as_object)
        .is_some_and(|servers| servers.contains_key(name));
    if elsewhere {
        bail!(
            "MCP server {name:?} is not in {} — it is defined in {} (use {} )",
            path.display(),
            other.display(),
            if local { "without --local" } else { "--local" }
        );
    }
    bail!("MCP server {name:?} not found in {}", path.display());
}

/// `tack mcp logout`: delete the cached OAuth token for a server.
pub fn logout(name: &str, agent_dir: &Path) -> Result<()> {
    if crate::mcp_oauth::delete_token(agent_dir, name)? {
        println!("Signed out of MCP server {name:?} (token cache entry removed).");
    } else {
        println!("MCP server {name:?} has no cached OAuth token.");
    }
    Ok(())
}

/// `tack mcp login`: run the interactive OAuth flow for a remote server.
/// The resulting token is cached; the next session/connect picks it up.
pub async fn login(name: &str, cwd: &Path, agent_dir: &Path) -> Result<()> {
    let specs = crate::mcp_config::configured_servers(cwd, agent_dir);
    let spec = specs
        .iter()
        .find(|s| s.name == name)
        .with_context(|| format!("MCP server {name:?} is not configured (see `tack mcp list`)"))?;
    let Some(url) = spec.url() else {
        bail!("MCP server {name:?} is a stdio server; OAuth applies to remote (url) servers");
    };
    let oauth = crate::mcp_oauth::spec_oauth(spec);
    let token = crate::mcp_oauth::authorize(agent_dir, name, url, &oauth).await?;
    // Verify immediately: a failed connect after a successful sign-in
    // means the problem is the server, not the credentials.
    let verify = spec.clone().with_extra_headers(vec![(
        "authorization".to_string(),
        format!("Bearer {token}"),
    )]);
    match tack_tools::mcp::connect_with(&verify, Default::default()).await {
        Ok(conn) => {
            let tools = conn.tools().len();
            conn.cancel();
            println!("MCP server {name:?}: signed in and connected ({tools} tools).");
        }
        Err(e) => {
            println!("MCP server {name:?}: signed in, but connect failed: {e}");
        }
    }
    Ok(())
}

/// One row of `tack mcp list` output before connecting.
struct ListRow {
    name: String,
    source: &'static str,
    spec: Option<tack_tools::mcp::McpServerSpec>,
    /// Config error for malformed entries (never connected).
    invalid: Option<String>,
}

/// Load both config files preserving provenance (project entries shadow
/// user entries of the same name). Malformed entries become rows with an
/// error instead of vanishing — `list` is the diagnostic surface.
fn list_rows(cwd: &Path, agent_dir: &Path) -> Vec<ListRow> {
    let mut rows: std::collections::BTreeMap<String, ListRow> = std::collections::BTreeMap::new();
    let mut load = |path: &Path, source: &'static str| {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(Value::Object(doc)) = serde_json::from_str::<Value>(&content) else {
            rows.insert(
                path.display().to_string(),
                ListRow {
                    name: path.display().to_string(),
                    source,
                    spec: None,
                    invalid: Some("file is not valid JSON".to_string()),
                },
            );
            return;
        };
        let Some(servers) = doc.get("mcpServers").and_then(Value::as_object) else {
            return;
        };
        for (name, entry) in servers {
            match crate::mcp_config::spec_from_entry(name, entry, &path.display().to_string()) {
                Some(spec) => {
                    rows.insert(
                        name.clone(),
                        ListRow {
                            name: name.clone(),
                            source,
                            spec: Some(spec),
                            invalid: None,
                        },
                    );
                }
                None => {
                    rows.insert(
                        name.clone(),
                        ListRow {
                            name: name.clone(),
                            source,
                            spec: None,
                            invalid: Some(
                                "invalid entry (needs \"command\" or \"url\"; see warnings above)"
                                    .to_string(),
                            ),
                        },
                    );
                }
            }
        }
    };
    load(&user_mcp_path(agent_dir), "user");
    // Project MCP servers can execute arbitrary commands — trust-gated,
    // same as the session paths.
    if crate::project_trust::is_trusted(cwd, agent_dir) {
        load(&project_mcp_path(cwd), "project");
    }
    rows.into_values().collect()
}

fn transport_label(spec: &tack_tools::mcp::McpServerSpec) -> &'static str {
    match &spec.transport {
        tack_tools::mcp::McpTransport::Stdio { .. } => "stdio",
        tack_tools::mcp::McpTransport::Http { .. } => "http",
        tack_tools::mcp::McpTransport::Sse { .. } => "sse",
    }
}

/// `tack mcp list`: show every configured server with its source and
/// connection state, connecting to every enabled server (cached OAuth
/// tokens only — never an interactive prompt). Exit status 1 when an
/// entry is invalid or an enabled server is not connected.
pub async fn list(cwd: &Path, agent_dir: &Path) -> Result<()> {
    let rows = list_rows(cwd, agent_dir);
    if rows.is_empty() {
        println!(
            "No MCP servers configured. Add one with `tack mcp add` (user: {}, project: {}).",
            user_mcp_path(agent_dir).display(),
            project_mcp_path(cwd).display(),
        );
        return Ok(());
    }
    let enabled: Vec<tack_tools::mcp::McpServerSpec> = rows
        .iter()
        .filter_map(|row| row.spec.clone())
        .filter(|spec| spec.enabled)
        .collect();
    let outcomes = crate::mcp_oauth::connect_all_oauth_reporting(
        enabled,
        agent_dir,
        false,
        Default::default(),
    )
    .await;
    let mut failed = false;
    for row in &rows {
        if let Some(error) = &row.invalid {
            failed = true;
            println!("{}\t{}\tINVALID: {error}", row.name, row.source);
            continue;
        }
        let Some(spec) = &row.spec else {
            continue;
        };
        if !spec.enabled {
            println!(
                "{}\t{}\t{}\tdisabled",
                row.name,
                row.source,
                transport_label(spec)
            );
            continue;
        }
        let exposure_suffix = match spec.exposure {
            tack_tools::mcp::McpExposure::Direct => String::new(),
            other => format!("\texposure={other:?}"),
        }
        .to_lowercase();
        match outcomes.iter().find(|o| o.name == row.name) {
            Some(outcome) => match &outcome.result {
                Ok(conn) => {
                    let tools = conn.tools();
                    println!(
                        "{}\t{}\t{}\tconnected ({} tools){exposure_suffix}",
                        row.name,
                        row.source,
                        transport_label(spec),
                        tools.len()
                    );
                    for tool in &tools {
                        let mut marker = "";
                        if spec.effective_exposure(&tool.name)
                            != tack_tools::mcp::McpExposure::Direct
                        {
                            marker = match spec.effective_exposure(&tool.name) {
                                tack_tools::mcp::McpExposure::Deferred => " [deferred]",
                                tack_tools::mcp::McpExposure::Hidden => " [hidden]",
                                tack_tools::mcp::McpExposure::Direct => "",
                            };
                        }
                        if marker.is_empty()
                            && let Some(a) = tool.annotations.as_ref()
                        {
                            if a.read_only_hint == Some(true) {
                                marker = " [read-only]";
                            } else if a.destructive_hint == Some(true) {
                                marker = " [destructive]";
                            }
                        }
                        let desc = tool
                            .description
                            .as_deref()
                            .unwrap_or("")
                            .lines()
                            .next()
                            .unwrap_or("");
                        println!("    {} — {}{marker}", tool.name, desc);
                    }
                    conn.cancel();
                }
                Err(e) => {
                    failed = true;
                    let hint = if e.contains("401") || e.to_lowercase().contains("unauthor") {
                        format!(" (sign in with `tack mcp login {}`)", row.name)
                    } else {
                        String::new()
                    };
                    println!(
                        "{}\t{}\t{}\tFAILED: {e}{hint}",
                        row.name,
                        row.source,
                        transport_label(spec)
                    );
                }
            },
            None => {
                failed = true;
                println!(
                    "{}\t{}\t{}\tFAILED: not attempted",
                    row.name,
                    row.source,
                    transport_label(spec)
                );
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn upsert_preserves_unrelated_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers": {"keep": {"command": "k"}}, "otherKey": 42}"#,
        )
        .unwrap();
        upsert_server(
            &path,
            "new",
            serde_json::json!({"url": "http://localhost:1/mcp"}),
        )
        .unwrap();
        let doc = load_json_object(&path).unwrap();
        let servers = doc.get("mcpServers").unwrap().as_object().unwrap();
        assert!(servers.contains_key("keep"), "unrelated server preserved");
        assert!(servers.contains_key("new"));
        assert_eq!(doc.get("otherKey"), Some(&serde_json::json!(42)));

        // Replace in place.
        upsert_server(&path, "new", serde_json::json!({"command": "c2"})).unwrap();
        let doc = load_json_object(&path).unwrap();
        assert_eq!(doc["mcpServers"]["new"]["command"], serde_json::json!("c2"));
        assert!(doc["mcpServers"].get("keep").is_some());
    }

    #[test]
    fn remove_only_the_named_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers": {"a": {"command": "a"}, "b": {"command": "b"}}}"#,
        )
        .unwrap();
        assert!(remove_server(&path, "a").unwrap());
        assert!(
            !remove_server(&path, "a").unwrap(),
            "second remove is a no-op"
        );
        let doc = load_json_object(&path).unwrap();
        let servers = doc["mcpServers"].as_object().unwrap();
        assert!(!servers.contains_key("a"));
        assert!(servers.contains_key("b"));
    }

    #[test]
    fn malformed_file_is_not_clobbered() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(upsert_server(&path, "x", serde_json::json!({})).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn name_validation_matches_other_clients() {
        assert!(validate_name("github-2_x").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("bad name").is_err());
        assert!(validate_name("bad.name").is_err());
    }

    #[test]
    fn add_writes_stdio_and_remote_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();

        // stdio via trailing command.
        add(
            &AddArgs {
                name: "fs".to_string(),
                local: false,
                url: None,
                transport: None,
                env: vec!["KEY=value".to_string()],
                headers: vec![],
                bearer_token_env_var: None,
                command: vec!["npx".to_string(), "-y".to_string(), "srv".to_string()],
            },
            cwd,
            &agent,
        )
        .unwrap();
        let doc = load_json_object(&agent.join("mcp.json")).unwrap();
        let fs = &doc["mcpServers"]["fs"];
        assert_eq!(fs["command"], serde_json::json!("npx"));
        assert_eq!(fs["args"], serde_json::json!(["-y", "srv"]));
        assert_eq!(fs["env"]["KEY"], serde_json::json!("value"));

        // remote with bearer token env var + project scope.
        add(
            &AddArgs {
                name: "docs".to_string(),
                local: true,
                url: Some("https://example.com/mcp".to_string()),
                transport: None,
                env: vec![],
                headers: vec![],
                bearer_token_env_var: Some("DOCS_TOKEN".to_string()),
                command: vec![],
            },
            cwd,
            &agent,
        )
        .unwrap();
        let doc = load_json_object(&cwd.join(".pi").join("mcp.json")).unwrap();
        let docs = &doc["mcpServers"]["docs"];
        assert_eq!(docs["url"], serde_json::json!("https://example.com/mcp"));
        assert_eq!(
            docs["headers"]["Authorization"],
            serde_json::json!("Bearer ${DOCS_TOKEN}")
        );
    }

    #[test]
    fn logout_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path();
        // No token file at all: a clean no-op.
        logout("unknown", agent).unwrap();
    }
}
