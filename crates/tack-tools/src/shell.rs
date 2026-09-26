//! Shell resolution and process management. Port of
//! `packages/coding-agent/src/utils/shell.ts`.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandTransport {
    /// Command passed as a `-c` argument.
    Argv,
    /// Command piped via stdin with `-s` (legacy WSL bash workaround).
    Stdin,
}

#[derive(Clone, Debug)]
pub struct ShellConfig {
    pub shell: PathBuf,
    pub args: Vec<String>,
    pub transport: CommandTransport,
}

impl ShellConfig {
    /// bash specifically (for bash-only setup like `set -o pipefail`):
    /// matches /bin/bash, /usr/local/bin/bash and git-bash's bash.exe,
    /// but not sh/dash where the flag would error out.
    pub fn is_bash(&self) -> bool {
        self.shell
            .file_stem()
            .is_some_and(|s| s.eq_ignore_ascii_case("bash"))
    }
}

/// Spell a filesystem path as a single shell word. On Windows `display()`
/// yields backslashes, which Git Bash (the resolved shell there) consumes
/// as escape characters — `C:\a\b` must reach the shell as `C:/a/b`;
/// single quotes additionally cover spaces.
pub fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\\', "/"))
}

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("Custom shell path not found: {0}")]
    CustomNotFound(String),
    #[error("{0}")]
    NoShell(String),
}

fn is_legacy_wsl_bash_path(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('/', "\\").to_lowercase();
    // ^[a-z]:\\windows\\(system32|sysnative)\\bash\.exe$
    let bytes = normalized.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    normalized.ends_with("\\windows\\system32\\bash.exe")
        || normalized.ends_with("\\windows\\sysnative\\bash.exe")
}

fn bash_shell_config(shell: PathBuf) -> ShellConfig {
    if is_legacy_wsl_bash_path(&shell) {
        ShellConfig {
            shell,
            args: vec!["-s".to_string()],
            transport: CommandTransport::Stdin,
        }
    } else {
        ShellConfig {
            shell,
            args: vec!["-c".to_string()],
            transport: CommandTransport::Argv,
        }
    }
}

/// PowerShell invocation args (TS `POWERSHELL_ARGS`):
/// no profile, non-interactive, execution policy bypassed.
pub const POWERSHELL_ARGS: [&str; 5] = [
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
];

/// ShellConfig for a resolved PowerShell executable.
pub fn powershell_shell_config(shell: PathBuf) -> ShellConfig {
    ShellConfig {
        shell,
        args: POWERSHELL_ARGS.iter().map(|s| s.to_string()).collect(),
        transport: CommandTransport::Argv,
    }
}

#[cfg(windows)]
fn find_executable_on_path(executable: &str) -> Option<PathBuf> {
    // `where` can return non-existent paths; verify existence.
    let output = std::process::Command::new("where")
        .arg(executable)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = stdout.lines().next()?.trim();
    let path = PathBuf::from(first);
    path.exists().then_some(path)
}

/// Resolve PowerShell (TS `getPowerShellConfig`): Windows-only, preferring
/// PowerShell 7 (`pwsh.exe`) over Windows PowerShell (`powershell.exe`).
/// On other platforms this is an error — the powershell tool registers
/// fine but fails here at execution time, matching TS semantics.
pub fn resolve_powershell() -> Result<ShellConfig, ShellError> {
    #[cfg(not(windows))]
    {
        Err(ShellError::NoShell(
            "The powershell tool is only available on Windows.".to_string(),
        ))
    }
    #[cfg(windows)]
    {
        let shell = find_executable_on_path("pwsh.exe")
            .or_else(|| find_executable_on_path("powershell.exe"))
            .ok_or_else(|| {
                ShellError::NoShell(
                    "No PowerShell executable found. Install PowerShell or add powershell.exe/pwsh.exe to PATH."
                        .to_string(),
                )
            })?;
        Ok(powershell_shell_config(shell))
    }
}

#[cfg(windows)]
fn find_bash_on_path() -> Option<PathBuf> {
    find_executable_on_path("bash.exe")
}

#[cfg(not(windows))]
fn find_bash_on_path() -> Option<PathBuf> {
    let output = std::process::Command::new("which")
        .arg("bash")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = stdout.lines().next()?.trim();
    (!first.is_empty()).then(|| PathBuf::from(first))
}

/// Resolve the shell configuration:
/// 1. user-specified path
/// 2. Windows: Git Bash in known locations, then bash on PATH
/// 3. Unix: /bin/bash, then bash on PATH, then sh
pub fn resolve_shell(custom_shell_path: Option<&Path>) -> Result<ShellConfig, ShellError> {
    if let Some(custom) = custom_shell_path {
        if custom.exists() {
            return Ok(bash_shell_config(custom.to_path_buf()));
        }
        return Err(ShellError::CustomNotFound(custom.display().to_string()));
    }

    #[cfg(windows)]
    {
        let mut searched: Vec<String> = Vec::new();
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(dir) = std::env::var(var) {
                let path = PathBuf::from(dir).join("Git").join("bin").join("bash.exe");
                searched.push(path.display().to_string());
                if path.exists() {
                    return Ok(bash_shell_config(path));
                }
            }
        }
        if let Some(bash) = find_bash_on_path() {
            return Ok(bash_shell_config(bash));
        }
        Err(ShellError::NoShell(format!(
            "No bash shell found. Options:\n  1. Install Git for Windows: https://git-scm.com/download/win\n  2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  3. Set shellPath in settings.json\n\nSearched Git Bash in:\n{}",
            searched
                .iter()
                .map(|p| format!("  {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        )))
    }

    #[cfg(not(windows))]
    {
        let bin_bash = PathBuf::from("/bin/bash");
        if bin_bash.exists() {
            return Ok(bash_shell_config(bin_bash));
        }
        if let Some(bash) = find_bash_on_path() {
            return Ok(bash_shell_config(bash));
        }
        Ok(ShellConfig {
            shell: PathBuf::from("sh"),
            args: vec!["-c".to_string()],
            transport: CommandTransport::Argv,
        })
    }
}

/// Managed binaries directory (`<agent dir>/bin`) where tack-app's tools
/// manager installs fd/rg. Mirrors `tack_session::default_agent_dir` — kept
/// local to avoid a tack-tools → tack-session dependency.
pub fn managed_bin_dir() -> PathBuf {
    let agent = std::env::var_os("TACK_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".tack")
                .join("agent")
        });
    agent.join("bin")
}

/// PATH with the managed bin dir prepended (TS getShellEnv): agent bash
/// commands find downloaded rg/fd. Returns `(key, value)` — the key keeps
/// the platform's original casing ("Path" on Windows).
pub fn path_with_managed_bin() -> Option<(String, std::ffi::OsString)> {
    let bin = managed_bin_dir();
    let key = std::env::vars_os()
        .map(|(k, _)| k)
        .find(|k| k.to_string_lossy().eq_ignore_ascii_case("path"))
        .map(|k| k.to_string_lossy().to_string())
        .unwrap_or_else(|| "PATH".to_string());
    let current = std::env::var_os(&key).unwrap_or_default();
    let mut entries: Vec<std::ffi::OsString> = std::env::split_paths(&current)
        .map(|p| p.into_os_string())
        .collect();
    if entries.iter().any(|p| Path::new(p) == bin) {
        return None;
    }
    entries.insert(0, bin.into_os_string());
    let joined = std::env::join_paths(&entries).ok()?;
    Some((key, joined))
}

/// Kill a process and all its children (cross-platform).
pub fn kill_process_tree(pid: u32) {
    // Guard the pid-0 footgun: callers use `child.id().unwrap_or(0)`, and
    // on Unix `kill -KILL -0` targets OUR OWN process group — that would
    // SIGKILL tack itself. No-op instead.
    if pid == 0 {
        tracing::warn!("kill_process_tree called with pid 0; ignoring");
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        // Direct syscalls via nix, not the `kill` binary: procps is
        // absent in minimal environments (slim docker images, Termux),
        // and `Command::new("kill")` then fails SILENTLY — leaving the
        // very child this function exists to kill running (its own
        // timeout semantics defeated; observed as 60s hangs inside the
        // CI container).
        let raw = pid as i32;
        // Negative pid kills the process group; fall back to the single
        // pid when there is no such group.
        let group = nix::unistd::Pid::from_raw(-raw);
        if nix::sys::signal::kill(group, nix::sys::signal::Signal::SIGKILL).is_err() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(raw),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

/// Sanitize binary output for display/storage: drop control characters
/// (except tab/newline/CR) and Unicode format characters U+FFF9..U+FFFB.
pub fn sanitize_binary_output(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            let code = c as u32;
            if code == 0x09 || code == 0x0a || code == 0x0d {
                return true;
            }
            if code <= 0x1f {
                return false;
            }
            !(0xfff9..=0xfffb).contains(&code)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn legacy_wsl_bash_path_detection() {
        assert!(is_legacy_wsl_bash_path(Path::new(
            "C:\\Windows\\System32\\bash.exe"
        )));
        assert!(is_legacy_wsl_bash_path(Path::new(
            "c:/windows/sysnative/bash.exe"
        )));
        assert!(is_legacy_wsl_bash_path(Path::new(
            "D:\\WINDOWS\\SYSTEM32\\BASH.EXE"
        )));
        // Git Bash and friends must use the normal -c transport.
        assert!(!is_legacy_wsl_bash_path(Path::new(
            "C:\\Program Files\\Git\\bin\\bash.exe"
        )));
        assert!(!is_legacy_wsl_bash_path(Path::new("/bin/bash")));
        assert!(!is_legacy_wsl_bash_path(Path::new(
            "C:\\tools\\msys64\\usr\\bin\\bash.exe"
        )));
        // Not a drive-letter path.
        assert!(!is_legacy_wsl_bash_path(Path::new(
            "\\windows\\system32\\bash.exe"
        )));
    }

    /// kill_process_tree relies on the victim being a process-group leader
    /// (callers set process_group(0)): the group kill must reap children
    /// that outlive the leader's own lifetime. Guards the mechanism
    /// browser::dump_dom and the bash executor depend on for zombie
    /// reaping — without the group, only the leader would die.
    #[cfg(unix)]
    #[test]
    fn kill_process_tree_reaps_group_children() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;

        let mut leader = std::process::Command::new("bash")
            .args(["-c", "sleep 300 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let stdout = leader.stdout.take().unwrap();
        let grandchild_pid: u32 = BufRead::lines(std::io::BufReader::new(stdout))
            .next()
            .unwrap()
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        kill_process_tree(leader.id());
        let _ = leader.wait();

        // SIGKILL delivery is synchronous for the group, but reap state
        // lags; poll briefly instead of asserting instantly.
        let mut alive = true;
        for _ in 0..50 {
            alive = std::process::Command::new("kill")
                .args(["-0", &grandchild_pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(!alive, "grandchild {grandchild_pid} survived group kill");
    }

    /// Regression: pid 0 must be a no-op — on Unix `kill -KILL -0` would
    /// target OUR OWN process group (callers use child.id().unwrap_or(0)),
    /// i.e. SIGKILL tack itself. This test surviving is the assertion.
    #[test]
    fn kill_process_tree_pid_zero_is_noop() {
        kill_process_tree(0);
    }

    /// Sanitization keeps \t \n \r, drops other C0 controls and the
    /// Unicode interlinear annotation anchors.
    #[test]
    fn sanitize_drops_control_chars() {
        assert_eq!(sanitize_binary_output("a\u{0}b\u{7}c"), "abc");
        assert_eq!(sanitize_binary_output("a\tb\nc\rd"), "a\tb\nc\rd");
        assert_eq!(
            sanitize_binary_output("x\u{FFF9}y\u{FFFA}z\u{FFFB}w"),
            "xyzw"
        );
    }

    /// PowerShell argv construction (pure — testable off-Windows).
    #[test]
    fn powershell_shell_config_args() {
        let config = powershell_shell_config(PathBuf::from("C:\\Tools\\pwsh.exe"));
        assert_eq!(
            config.args,
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command"
            ]
        );
        assert_eq!(config.transport, CommandTransport::Argv);
        assert_eq!(config.shell, PathBuf::from("C:\\Tools\\pwsh.exe"));
    }

    /// TS getPowerShellConfig throws off-Windows; resolution must fail with
    /// the same message (the tool itself still registers — see powershell.rs).
    #[cfg(not(windows))]
    #[test]
    fn powershell_resolution_errors_off_windows() {
        let err = resolve_powershell().unwrap_err();
        assert!(
            err.to_string().contains("only available on Windows"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn is_bash_matches_bash_but_not_sh() {
        let bash = ShellConfig {
            shell: PathBuf::from("/bin/bash"),
            args: vec!["-c".to_string()],
            transport: CommandTransport::Argv,
        };
        assert!(bash.is_bash());
        let sh = ShellConfig {
            shell: PathBuf::from("sh"),
            args: vec!["-c".to_string()],
            transport: CommandTransport::Argv,
        };
        assert!(!sh.is_bash());
        let dash = ShellConfig {
            shell: PathBuf::from("/bin/dash"),
            args: vec!["-c".to_string()],
            transport: CommandTransport::Argv,
        };
        assert!(!dash.is_bash());
    }
}
