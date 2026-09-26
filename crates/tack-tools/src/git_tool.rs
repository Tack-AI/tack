//! The git tool: a sandboxed, permission-friendly front end for git. Unlike
//! raw `bash`, the tool parses the subcommand itself, so the permission
//! system can classify calls structurally (`Git(push*)` rules, plan-mode
//! read-only gating) instead of pattern-matching shell text that is easy to
//! bypass with `git -C`, aliases, or quoting.
//!
//! Safety model:
//! - Calls are parsed with a shell-style tokenizer (shell_split): quotes and
//!   backslash escapes produce literal argument values, and shell
//!   metacharacters are rejected ONLY outside quotes — so
//!   `commit -m "fix: a > b, see `note`"` works while `status && rm -rf /`
//!   does not. Every parsed argument is re-quoted (shell_quote) before the
//!   invocation reaches the execution shell, making quoted content inert.
//! - The first argument must be a known subcommand from the allowlist, which
//!   rules out config aliases (`alias.foo = !rm -rf /`) and global options
//!   like `-C`/`--git-dir`/`--exec-path` that redirect execution.
//! - Read-only subcommands (status/log/diff/show/...) are classified by
//!   `is_read_only_command` and run free in plan mode; everything else goes
//!   through the normal permission prompt. Dangerous history/state commands
//!   (push, reset, clean, ...) are never read-only.
//! - `commands: [...]` batches several invocations into one call (executed
//!   in order, stop at first failure); the batch is read-only only if EVERY
//!   entry is read-only.
//! - Execution reuses the bash executor, so OS sandboxing and ACP client
//!   terminals apply exactly as for bash.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tack_agent_core::{AgentTool, AgentToolResult, ToolExecutionMode};
use tokio_util::sync::CancellationToken;

use crate::executor::LocalBashExecutor;
use crate::services::ToolServices;

/// Subcommands the tool accepts. Anything else is rejected — this is what
/// blocks aliases and exotic plumbing.
const ALLOWED_SUBCOMMANDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "rev-parse",
    "ls-files",
    "describe",
    "shortlog",
    "cat-file",
    "grep",
    "reflog",
    "rev-list",
    "ls-tree",
    "ls-remote",
    "branch",
    "switch",
    "checkout",
    "restore",
    "add",
    "rm",
    "mv",
    "commit",
    "merge",
    "rebase",
    "cherry-pick",
    "revert",
    "tag",
    "stash",
    "fetch",
    "pull",
    "push",
    "remote",
    "worktree",
    "init",
    "clone",
    "config",
    "bisect",
];

/// Long options that turn a nominally read-only subcommand into a writer or
/// an external-command runner. Proven with real git:
/// - `--output=FILE` (log/diff/show/`stash list`): writes output to an
///   arbitrary file — a file-write primitive that would escape plan mode and
///   the sandbox's read-only assumption (and `../` escapes the repo).
/// - `--ext-diff` (diff/log/show): runs the configured `diff.external`
///   command — arbitrary code execution.
/// - `--textconv` (diff/log/show): runs configured `diff.<driver>.textconv`
///   filters — arbitrary code execution (driver selectable via the repo's
///   committed .gitattributes).
/// - `--open-files-in-pager[=<cmd>]` (grep): spawns the "pager" through the
///   shell — arbitrary code execution even under `--no-pager`.
/// - `--upload-pack=<cmd>` (ls-remote): runs <cmd> as the remote-side
///   process — arbitrary code execution dressed as a read.
///
/// Matching is by prefix because git accepts any unambiguous long-option
/// abbreviation (`--out=...`, `--textc`, `--open`), and `=value` forms are
/// split off first. Over-blocking is harmless: an arg that only
/// prefix-matches but is not one of these options would make git error out
/// anyway, so the call just falls back to a permission prompt.
const DANGEROUS_READ_LONG_OPTIONS: &[&str] = &[
    "--output",
    "--ext-diff",
    "--textconv",
    "--open-files-in-pager",
    "--exec",
    "--upload-pack",
];

fn is_dangerous_read_option(arg: &str) -> bool {
    if arg.starts_with("--") {
        let name = arg.split('=').next().unwrap_or(arg);
        return name.len() > 2
            && DANGEROUS_READ_LONG_OPTIONS
                .iter()
                .any(|opt| opt.starts_with(name));
    }
    // Short-option clusters: `git grep -O<cmd>` (also bundled, e.g. `-lOcmd`)
    // runs <cmd> through the shell as the "pager". Reject any cluster
    // containing `O`; the only other read-only use (`diff -O<orderfile>`) is
    // read-only and merely falls back to a prompt.
    arg.starts_with('-') && arg.len() > 1 && arg[1..].contains('O')
}

/// Split a command line into arguments with shell quoting rules: '...' is
/// fully literal, "..." allows \ and " escapes, \x outside quotes is a
/// literal x. Shell metacharacters (; | & < > ` newline, $(...) are rejected
/// ONLY when unquoted: every parsed argument is re-quoted (shell_quote)
/// before execution, so quoted content is inert — commit messages may
/// contain >, |, `, $(...) and friends.
fn shell_split(command: &str) -> Result<Vec<String>, String> {
    const REJECT: &str = "command must be a plain git invocation — shell metacharacters and redirection are not allowed outside quotes (quote literal text, e.g. commit -m \"a > b\")";
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Newlines are whitespace too — reject them explicitly first so
            // multi-line smuggling is reported as metacharacters, not split.
            ';' | '|' | '&' | '<' | '>' | '`' | '\n' | '\r' => return Err(REJECT.to_string()),
            '$' if chars.peek() == Some(&'(') => return Err(REJECT.to_string()),
            _ if c.is_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            '\\' => {
                in_arg = true;
                match chars.next() {
                    Some(escaped) => {
                        if matches!(escaped, ';' | '|' | '&' | '<' | '>' | '`' | '\n' | '\r') {
                            return Err(REJECT.to_string());
                        }
                        cur.push(escaped);
                    }
                    None => cur.push('\\'),
                }
            }
            '\'' => {
                in_arg = true;
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == '\'' {
                        closed = true;
                        break;
                    }
                    cur.push(c2);
                }
                if !closed {
                    return Err("unbalanced single quote".to_string());
                }
            }
            '"' => {
                in_arg = true;
                let mut closed = false;
                while let Some(c2) = chars.next() {
                    match c2 {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => match chars.next() {
                            Some(e @ ('"' | '\\')) => cur.push(e),
                            // Unknown escapes stay literal (\n = backslash + n).
                            Some(other) => {
                                cur.push('\\');
                                cur.push(other);
                            }
                            None => cur.push('\\'),
                        },
                        _ => cur.push(c2),
                    }
                }
                if !closed {
                    return Err("unbalanced double quote".to_string());
                }
            }
            _ => {
                in_arg = true;
                cur.push(c);
            }
        }
    }
    if in_arg {
        args.push(cur);
    }
    Ok(args)
}

/// Re-quote one parsed argument for the execution shell: safe characters
/// pass through bare, everything else is single-quoted (' becomes '\'').
fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._/:=%+,@^".contains(c))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// The exact shell line for one validated command: fixed git prefix plus
/// every parsed argument safely re-quoted.
fn build_invocation(command: &str) -> Result<String, String> {
    let args = shell_split(command)?;
    let mut line = String::from("git --no-pager --no-optional-locks");
    for arg in &args {
        line.push(' ');
        line.push_str(&shell_quote(arg));
    }
    Ok(line)
}

/// Subcommands (with argument constraints) that never modify anything.
/// Anything not listed here is treated as mutating by the permission layer.
pub fn is_read_only_command(command: &str) -> bool {
    // Defense in depth: never classify a string we would refuse to execute.
    if validate(command).is_err() {
        return false;
    }
    let Ok(args) = shell_split(command) else {
        return false;
    };
    let Some(sub) = args.first() else {
        return false;
    };
    if args[1..].iter().any(|a| is_dangerous_read_option(a)) {
        return false;
    }
    match sub.as_str() {
        "status" | "log" | "diff" | "show" | "blame" | "rev-parse" | "ls-files" | "describe"
        | "shortlog" | "cat-file" | "grep" | "rev-list" | "ls-tree" | "ls-remote" => true,
        // Listing forms only: `git branch`, `git branch -a -v --list`,
        // `git remote -v`, `git stash list`, `git tag` (no name = list).
        "branch" => {
            args.len() == 1
                || args[1..].iter().all(|a| {
                    matches!(
                        a.as_str(),
                        "--list" | "-a" | "-v" | "-vv" | "--all" | "-r" | "--remotes"
                    )
                })
        }
        "remote" => {
            args.len() == 1
                || args[1..]
                    .iter()
                    .all(|a| matches!(a.as_str(), "-v" | "--verbose"))
        }
        // Bare `git stash` is `git stash push` — it mutates the working tree.
        "stash" => args.get(1).is_some_and(|a| a == "list"),
        "tag" => args.len() == 1,
        // `reflog` = `reflog show`; expire/delete rewrite refs.
        "reflog" => args.len() == 1 || matches!(args[1].as_str(), "show" | "exists"),
        // Explicit read actions only: positional `config key value` writes,
        // so it (and any write flag) stays permission-gated.
        "config" => {
            const READ_ACTIONS: &[&str] = &["--get", "--get-all", "--get-regexp", "-l", "--list"];
            const WRITE_ACTIONS: &[&str] = &[
                "--add",
                "--set",
                "--unset",
                "--unset-all",
                "--replace-all",
                "--rename-section",
                "--remove-section",
                "-e",
                "--edit",
                "--fixed-value",
            ];
            let rest = &args[1..];
            rest.iter().any(|a| READ_ACTIONS.contains(&a.as_str()))
                && !rest.iter().any(|a| WRITE_ACTIONS.contains(&a.as_str()))
        }
        _ => false,
    }
}

/// Batch form (`commands: [...]`): read-only only if EVERY entry is.
pub fn is_read_only_commands(commands: &[String]) -> bool {
    !commands.is_empty() && commands.iter().all(|c| is_read_only_command(c))
}

/// Reject anything that is not a plain, shell-free `git <subcommand>` call.
fn validate(command: &str) -> Result<(), String> {
    let args = shell_split(command)?;
    let Some(sub) = args.first() else {
        return Err("command must not be empty".to_string());
    };
    if sub.starts_with('-') {
        return Err(format!(
            "global git options like \"{sub}\" are not allowed; run plain git subcommands only"
        ));
    }
    if !ALLOWED_SUBCOMMANDS.contains(&sub.as_str()) {
        return Err(format!(
            "git subcommand \"{sub}\" is not in the allowlist; allowed: {}",
            ALLOWED_SUBCOMMANDS.join(", ")
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GitParams {
    /// Git arguments starting with the subcommand, e.g. "status", "log --oneline -5",
    /// "add src/". Global options (-C, --git-dir, aliases) are rejected.
    command: Option<String>,
    /// Batch form: several independent invocations (same syntax as `command`),
    /// run in order with && semantics (stop at first failure). Each entry is
    /// validated on its own; the call is read-only only if EVERY entry is.
    commands: Option<Vec<String>>,
    /// Timeout in seconds (default 120).
    timeout: Option<f64>,
}

pub struct GitTool {
    services: ToolServices,
}

impl GitTool {
    pub fn new(services: ToolServices) -> Self {
        GitTool { services }
    }
}

impl std::fmt::Debug for GitTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitTool").finish()
    }
}

const DEFAULT_TIMEOUT_SECS: f64 = 120.0;

#[async_trait]
impl AgentTool for GitTool {
    fn name(&self) -> &'static str {
        "git"
    }

    fn label(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "Run git commands in the current working directory. Provide 'command' (arguments starting \
         with the subcommand, e.g. \"status\", \"diff --stat\", \"add -A\", \"commit -m msg\") or \
         'commands' (an array of such invocations run in order — batch read-only queries like \
         [\"status\", \"log --oneline -10\"] into one call). Quote arguments containing special \
         characters (commit -m \"fix: a > b\"); shell metacharacters outside quotes, global \
         options (-C, --git-dir, ...) and unknown subcommands are rejected. Prefer this over \
         bash for git work."
    }

    fn parameters_schema(&self) -> Value {
        crate::schema_for::<GitParams>()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let params: GitParams =
            serde_json::from_value(params).map_err(|e| format!("invalid git params: {e}"))?;
        let commands = match (params.command, params.commands) {
            (Some(c), None) => vec![c],
            (None, Some(cs)) if !cs.is_empty() => cs,
            (None, Some(_)) => return Err("'commands' must not be empty".to_string()),
            (None, None) => return Err("provide 'command' or 'commands'".to_string()),
            (Some(_), Some(_)) => {
                return Err("provide exactly one of 'command' or 'commands'".to_string());
            }
        };
        for command in &commands {
            validate(command)?;
        }
        let timeout = match params.timeout {
            None => std::time::Duration::from_secs_f64(DEFAULT_TIMEOUT_SECS),
            Some(t) if !t.is_finite() || t <= 0.0 => {
                return Err("Invalid timeout: must be a finite number of seconds".to_string());
            }
            Some(t) => std::time::Duration::from_secs_f64(t.min(3_600.0)),
        };

        // `--no-pager` keeps output non-interactive; `--no-optional-locks`
        // avoids background index writes for read-only commands. Batches run
        // with && semantics: a failing entry stops the rest.
        let full = commands
            .iter()
            .map(|c| build_invocation(c))
            .collect::<Result<Vec<_>, _>>()?
            .join(" && ");

        let executor = match &self.services.bash_executor {
            Some(custom) => custom.clone(),
            None => {
                let shell = self
                    .services
                    .shell
                    .clone()
                    .ok_or_else(|| "no shell configured for git tool".to_string())?;
                std::sync::Arc::new(LocalBashExecutor {
                    shell,
                    sandbox: self
                        .services
                        .sandbox
                        .as_ref()
                        .and_then(crate::sandbox::resolve),
                    env: self.services.env.clone(),
                })
            }
        };

        // Stream merged output through the same accumulator bash uses.
        let output = std::sync::Arc::new(std::sync::Mutex::new(
            crate::accumulator::OutputAccumulator::new("tack-git"),
        ));
        let sink = output.clone();
        let outcome = executor
            .exec(
                &full,
                &self.services.cwd,
                Some(timeout),
                cancel,
                &move |data: &[u8]| {
                    sink.lock().unwrap_or_else(|e| e.into_inner()).append(data);
                },
            )
            .await?;

        let snapshot = output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot(true);
        let text = crate::shell::sanitize_binary_output(&snapshot.content);
        let text = if text.is_empty() {
            "(no output)".to_string()
        } else {
            text
        };
        if outcome.cancelled {
            return Err("git command aborted".to_string());
        }
        if outcome.timed_out {
            return Err(format!(
                "git command timed out after {} seconds",
                params.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS) as u64
            ));
        }
        if let Some(code) = outcome.exit_code
            && code != 0
        {
            return Err(format!("{text}\n\ngit exited with code {code}"));
        }

        Ok(AgentToolResult {
            content: vec![tack_ai::InputContentBlock::text(text)],
            details: serde_json::json!({
                "commands": commands,
                "readOnly": is_read_only_commands(&commands),
            }),
            usage: None,
            terminate: false,
            added_tool_names: None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn validation_rejects_shell_and_global_options() {
        assert!(validate("status").is_ok());
        assert!(validate("log --oneline -5").is_ok());
        assert!(validate("commit -m 'fix thing'").is_ok());
        for bad in [
            "status; rm -rf /",
            "log && echo pwned",
            "-C /etc status",
            "push `evil`",
            "status > /tmp/x",
            "status | tee /tmp/x",
            "status\nreboot",
            "alias-thing",
            "!sh",
            "",
        ] {
            assert!(validate(bad).is_err(), "expected rejection: {bad:?}");
        }
    }

    #[test]
    fn shell_split_honors_quotes() {
        assert_eq!(shell_split("status").unwrap(), vec!["status"]);
        assert_eq!(
            shell_split("commit -m 'a b'").unwrap(),
            vec!["commit", "-m", "a b"]
        );
        assert_eq!(
            shell_split("commit -m \"a b\"").unwrap(),
            vec!["commit", "-m", "a b"]
        );
        // Adjacent quoted/unquoted segments join into one argument.
        assert_eq!(
            shell_split("log --grep=\"a b\"c").unwrap(),
            vec!["log", "--grep=a bc"]
        );
        // Escapes outside quotes are literal.
        assert_eq!(shell_split("log a\\ b").unwrap(), vec!["log", "a b"]);
        // Empty quoted argument survives.
        assert_eq!(
            shell_split("commit -m \"\" --allow-empty").unwrap(),
            vec!["commit", "-m", "", "--allow-empty"]
        );
        // Unbalanced quotes are errors.
        assert!(shell_split("commit -m 'oops").is_err());
        assert!(shell_split("commit -m \"oops").is_err());
    }

    #[test]
    fn quoted_metacharacters_are_inert_content() {
        // The P2 regression: commit messages legitimately contain >, |, `,
        // $(...) — inside quotes they must be accepted and stay literal.
        for ok in [
            "commit -m \"fix: a > b\"",
            "commit -m 'use `Arc` here'",
            "commit -m \"ran $(cargo test), all green\"",
            "commit -m 'a | b && c; d < e > f'",
            "log --grep=\"a|b\"",
        ] {
            assert!(validate(ok).is_ok(), "expected acceptance: {ok:?}");
        }
        // The SAME characters unquoted are still rejected.
        for bad in [
            "commit -m fix: a > b",
            "log --grep=a|b",
            "status && echo pwned",
            "log $(whoami)",
        ] {
            assert!(validate(bad).is_err(), "expected rejection: {bad:?}");
        }
    }

    #[test]
    fn shell_quote_rebuilds_safe_invocations() {
        // Safe characters pass bare; everything else is single-quoted.
        assert_eq!(shell_quote("--oneline"), "--oneline");
        assert_eq!(shell_quote("HEAD~1"), "'HEAD~1'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
        // Embedded single quote: 'foo'bar' style escaping.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        // A rebuilt invocation carries no live metacharacters: the payload
        // is fully inside single quotes.
        let line = build_invocation("commit -m \"a > b `x` $(y)\"").unwrap();
        assert_eq!(
            line,
            "git --no-pager --no-optional-locks commit -m 'a > b `x` $(y)'"
        );
        // Single quotes inside the payload are escaped, not terminating.
        let line = build_invocation("commit -m \"it's done\"").unwrap();
        assert_eq!(
            line,
            "git --no-pager --no-optional-locks commit -m 'it'\\''s done'"
        );
    }

    #[test]
    fn read_only_classification_new_subcommands() {
        for ro in [
            "reflog",
            "reflog show",
            "reflog exists HEAD",
            "rev-list HEAD",
            "rev-list --count HEAD~5..HEAD",
            "ls-tree HEAD",
            "ls-tree -r --name-only HEAD",
            "ls-remote origin",
            "config --get user.email",
            "config --list",
            "config -l",
            "config --global --get-all alias.st",
        ] {
            assert!(is_read_only_command(ro), "expected read-only: {ro:?}");
        }
        for mutating in [
            "reflog expire --expire=now --all",
            "reflog delete HEAD@{1}",
            "config user.email t@t", // positional write
            "config --add alias.st status",
            "config --unset user.email",
            "config --get user.email --add alias.st status", // read + write flags
            "config -e",
            "ls-remote --upload-pack=evil origin", // remote-side exec
            "ls-remote --up=evil origin",          // abbreviation
        ] {
            assert!(
                !is_read_only_command(mutating),
                "expected mutating: {mutating:?}"
            );
        }
    }

    #[test]
    fn batch_classification_requires_all_read_only() {
        assert!(is_read_only_commands(&[
            "status".to_string(),
            "log --oneline -5".to_string()
        ]));
        assert!(!is_read_only_commands(&[
            "status".to_string(),
            "push origin main".to_string()
        ]));
        assert!(!is_read_only_commands(&[]));
    }

    #[test]
    fn params_require_exactly_one_form() {
        let services = crate::default_services(std::env::current_dir().unwrap());
        let tool = GitTool::new(services);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let noop = |_| {};
            for params in [
                serde_json::json!({}),
                serde_json::json!({"command": "status", "commands": ["status"]}),
                serde_json::json!({"commands": []}),
            ] {
                let err = tool
                    .execute("t", params.clone(), CancellationToken::new(), &noop)
                    .await
                    .unwrap_err();
                assert!(
                    err.contains("command"),
                    "params {params} should be rejected, got: {err}"
                );
            }
        });
    }

    /// Hermetic end-to-end: quoted metacharacters land in the commit message
    /// byte-for-byte, and a batch runs its entries in order.
    #[test]
    fn quoted_commit_and_batch_execution_roundtrip() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", tmp.path())
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        assert!(git(&["config", "user.email", "t@t"]).status.success());
        assert!(git(&["config", "user.name", "t"]).status.success());
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();

        let services = crate::default_services(repo.clone());
        if services.shell.is_none() {
            eprintln!("no shell available, skipping");
            return;
        }
        let tool = GitTool::new(services);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let noop = |_| {};
            // The payload would execute or redirect if quoting leaked.
            let msg = "fix: a > b `touch PWNED` $(touch PWNED2) it's";
            let result = tool
                .execute(
                    "1",
                    serde_json::json!({
                        "commands": [
                            "add a.txt",
                            format!("commit -qm \"{}\"", msg.replace('"', "\\\""))
                        ]
                    }),
                    CancellationToken::new(),
                    &noop,
                )
                .await
                .unwrap();
            assert_eq!(result.details["readOnly"], serde_json::json!(false));
            assert!(!repo.join("PWNED").exists(), "backtick payload executed");
            assert!(!repo.join("PWNED2").exists(), "$(...) payload executed");

            let out = git(&["log", "-1", "--pretty=%B"]);
            let subject = String::from_utf8_lossy(&out.stdout);
            assert_eq!(subject.trim_end(), msg, "message mangled in transit");

            // Batch: both entries run, output is concatenated, readOnly
            // reflects the whole batch.
            let batch = tool
                .execute(
                    "2",
                    serde_json::json!({"commands": ["status --short", "log --oneline -1"]}),
                    CancellationToken::new(),
                    &noop,
                )
                .await
                .unwrap();
            let text = match &batch.content[0] {
                tack_ai::InputContentBlock::Text { text, .. } => text.clone(),
                _ => panic!("expected text content"),
            };
            assert!(text.contains("fix: a > b"), "log output missing: {text}");
            assert_eq!(batch.details["readOnly"], serde_json::json!(true));

            // && semantics: a failing entry stops the batch and is an error.
            let err = tool
                .execute(
                    "3",
                    serde_json::json!({"commands": ["rev-parse --verify nope", "status"]}),
                    CancellationToken::new(),
                    &noop,
                )
                .await
                .unwrap_err();
            assert!(err.contains("exited with code"), "unexpected: {err}");
        });
    }

    #[test]
    fn read_only_classification() {
        for ro in [
            "status",
            "log --oneline -5",
            "diff HEAD~1",
            "show abc123",
            "blame main.rs",
            "branch",
            "branch -a -v",
            "branch --list",
            "remote -v",
            "stash list",
            "tag",
        ] {
            assert!(is_read_only_command(ro), "expected read-only: {ro:?}");
        }
        for mutating in [
            "push origin main",
            "reset --hard HEAD~1",
            "clean -fd",
            "branch -D feature",
            "tag v1.0",
            "stash", // bare stash = `stash push`: reverts the working tree
            "stash pop",
            "stash drop",
            "remote add origin url",
            "commit -m x",
            "add -A",
        ] {
            assert!(
                !is_read_only_command(mutating),
                "expected mutating: {mutating:?}"
            );
        }
    }

    #[test]
    fn read_only_rejects_write_and_exec_options() {
        // Each of these has a proven side effect with real git despite the
        // subcommand being a "read": --output writes arbitrary files,
        // --ext-diff/--textconv run configured external commands, grep -O
        // runs its argument through the shell as a pager.
        for dangerous in [
            "log --output=/tmp/x",
            "log --output /tmp/x",
            "log --out=/tmp/x",      // git accepts unambiguous abbreviations
            "diff --output=../../x", // also escapes the repo root
            "show --output HEAD",
            "stash list --output=/tmp/x",
            "diff --ext-diff",
            "log --ext-diff -p",
            "diff --ext", // abbreviation attempt
            "show --textconv HEAD",
            "diff --textc", // abbreviation attempt
            "grep --open-files-in-pager=evil pattern",
            "grep --open pattern",
            "grep -Oevil pattern",
            "grep -lOevil pattern", // bundled short options
            "grep -O evil pattern",
        ] {
            assert!(
                !is_read_only_command(dangerous),
                "expected NOT read-only: {dangerous:?}"
            );
        }
        // Disablers and lookalikes stay read-only.
        for safe in [
            "diff --no-ext-diff",
            "show --no-textconv HEAD",
            "log --oneline --decorate",
            "grep -n pattern",
        ] {
            assert!(is_read_only_command(safe), "expected read-only: {safe:?}");
        }
    }

    #[test]
    fn invalid_commands_are_never_read_only() {
        // The permission layer consults is_read_only_command without
        // validate(); the two must agree so a rejected string can never be
        // auto-approved as read-only.
        for bad in [
            "log; rm -rf /",
            "status && echo pwned",
            "status > /tmp/x",
            "-C /etc status",
            "nonsubcommand",
        ] {
            assert!(validate(bad).is_err());
            assert!(
                !is_read_only_command(bad),
                "expected NOT read-only: {bad:?}"
            );
        }
    }

    /// Hermetic proof (skipped when git is unavailable) that the blocklisted
    /// options really do have side effects — this is why classification
    /// alone cannot treat them as reads.
    #[test]
    fn dangerous_options_actually_write_and_execute() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", tmp.path())
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        assert!(git(&["config", "user.email", "t@t"]).status.success());
        assert!(git(&["config", "user.name", "t"]).status.success());
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        assert!(git(&["add", "a.txt"]).status.success());
        assert!(git(&["commit", "-qm", "init"]).status.success());

        // 1. `git log --output=FILE` writes a file (also outside the repo).
        let out = tmp.path().join("log_out.txt");
        assert!(
            git(&[
                "--no-pager",
                "log",
                &format!("--output={}", out.display()),
                "HEAD"
            ])
            .status
            .success()
        );
        assert!(out.exists(), "git log --output wrote a file");

        // 2. `git grep -O<cmd>` executes <cmd> through the shell, even under
        //    --no-pager. The pager goes through sh (git bundles MSYS sh on
        //    Windows), so forward-slash + quote the path: backslashes would
        //    be eaten as escapes and the write would land in a junk name.
        let pwned = tmp.path().join("pager_pwned");
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        let status = git(&[
            "--no-pager",
            "grep",
            &format!("-Otouch '{}'", pwned.to_string_lossy().replace('\\', "/")),
            "hello",
        ])
        .status;
        assert!(status.success());
        assert!(pwned.exists(), "git grep -O executed its argument");

        // 3. Bare `git stash` mutates the working tree.
        std::fs::write(repo.join("a.txt"), "dirty\n").unwrap();
        assert!(
            git(&["--no-pager", "--no-optional-locks", "stash"])
                .status
                .success()
        );
        let list = git(&["stash", "list"]);
        assert!(
            String::from_utf8_lossy(&list.stdout).contains("stash@"),
            "bare git stash created a stash entry"
        );
    }
}
