//! OS-level sandboxing for bash execution. Application-level permission
//! modes decide WHETHER a command may run; the sandbox limits WHAT it can
//! touch when it does (read-only system, writes confined to the workspace,
//! optional network cut-off).
//!
//! Backends (auto-detected, graceful degradation):
//!   Linux  — bubblewrap (`bwrap`) when on PATH
//!   macOS  — `sandbox-exec` (seatbelt) at /usr/bin/sandbox-exec
//!   Windows/other — unavailable: commands run unsandboxed (the host warns
//!   once at startup when sandboxing was requested).

use std::path::{Path, PathBuf};

use crate::shell::ShellConfig;

/// What the sandbox should enforce.
#[derive(Clone, Debug)]
pub struct SandboxSpec {
    /// Directories the command may write to (defaults to the cwd).
    pub writable: Vec<PathBuf>,
    /// Allow network access (default true).
    pub network: bool,
    /// Windows Job Object: cap on concurrent processes in the tree.
    pub max_processes: Option<u32>,
    /// Windows Job Object: cap on total job memory (MB).
    pub max_memory_mb: Option<u64>,
}

impl Default for SandboxSpec {
    fn default() -> Self {
        SandboxSpec {
            writable: Vec::new(),
            network: true,
            max_processes: None,
            max_memory_mb: None,
        }
    }
}

impl SandboxSpec {
    pub fn for_cwd(cwd: &Path) -> Self {
        SandboxSpec {
            writable: vec![cwd.to_path_buf()],
            ..Default::default()
        }
    }
}

/// A detected platform sandbox backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxBackend {
    Bubblewrap(PathBuf),
    Seatbelt(PathBuf),
    /// Windows Job Objects (resource containment, not a security boundary).
    WindowsJob,
}

/// Detect the available sandbox backend for this platform, if any.
pub fn detect() -> Option<SandboxBackend> {
    #[cfg(target_os = "linux")]
    {
        if let Some(bwrap) = which("bwrap") {
            return Some(SandboxBackend::Bubblewrap(bwrap));
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let seatbelt = PathBuf::from("/usr/bin/sandbox-exec");
        seatbelt
            .exists()
            .then_some(SandboxBackend::Seatbelt(seatbelt))
    }
    #[cfg(windows)]
    {
        // Job Objects are always available (kernel feature since Win2000).
        Some(SandboxBackend::WindowsJob)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn which(program: &str) -> Option<PathBuf> {
    let output = std::process::Command::new("which")
        .arg(program)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let first = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    (!first.is_empty()).then(|| PathBuf::from(first))
}

/// Escape a path for embedding in an SBPL (seatbelt profile) string
/// literal. A raw `"` would terminate the string and break (or inject
/// into) the profile; raw control characters could inject whole profile
/// lines. Escapes that sandbox-exec doesn't interpret simply never match
/// a path — fail-closed, unlike dropping characters (which could
/// accidentally target a different existing path).
fn sbpl_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Roots every backend makes writable by default (seatbelt allows them
/// literally, bwrap gets `--tmpfs /tmp` + `--dev /dev`). Shared between
/// the profile builder and [`writable_covers`] so the two never drift.
const BUILTIN_WRITABLE: &[&str] = &["/tmp", "/private/tmp", "/private/var/folders", "/dev"];

/// True when `dir` is writable under `spec` (a declared writable root or
/// a built-in allowance). Symlink-aware like the seatbelt profile: /tmp
/// is covered even though the profile spells it /private/tmp too.
pub fn writable_covers(spec: &SandboxSpec, dir: &std::path::Path) -> bool {
    let resolved = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    spec.writable
        .iter()
        .map(|d| dunce::canonicalize(d).unwrap_or_else(|_| d.clone()))
        .chain(BUILTIN_WRITABLE.iter().map(std::path::PathBuf::from))
        .any(|root| resolved == root || resolved.starts_with(&root))
}

/// CARGO_HOME fallback for sandboxed commands. The default `~/.cargo` is
/// almost never inside the writable set, so under a sandbox every cargo
/// invocation dies with EPERM (builds, benches, clippy — a steady bleed
/// in long agent sessions). Point cargo at `tack-cargo-home` in the temp
/// dir instead: writable under every backend, persistent across a
/// session; if the OS reaps it, cargo simply refetches.
///
/// None when unsandboxed, when CARGO_HOME is already set (explicit user
/// config wins — a wrong one surfaces via [`denial_hint`]), or when the
/// default home is already covered by a writable root.
pub fn cargo_home_env(spec: Option<&SandboxSpec>) -> Option<(&'static str, std::path::PathBuf)> {
    let home = dirs::home_dir().map(|h| h.join(".cargo"));
    cargo_home_env_inner(
        spec,
        std::env::var_os("CARGO_HOME").is_some(),
        home.as_deref(),
    )
}

/// Pure core of [`cargo_home_env`] (env reading hoisted for testability).
fn cargo_home_env_inner(
    spec: Option<&SandboxSpec>,
    cargo_home_set: bool,
    default_home: Option<&std::path::Path>,
) -> Option<(&'static str, std::path::PathBuf)> {
    let spec = spec?;
    if cargo_home_set {
        return None;
    }
    let home = default_home?;
    if writable_covers(spec, home) {
        return None;
    }
    let fallback = std::env::temp_dir().join("tack-cargo-home");
    if let Err(e) = std::fs::create_dir_all(&fallback) {
        tracing::warn!(
            "sandbox: cannot create fallback CARGO_HOME {}: {e}",
            fallback.display()
        );
        return None;
    }
    Some(("CARGO_HOME", fallback))
}

/// Denial diagnosis for sandboxed commands. A sandboxed write fails as a
/// plain EPERM inside the failing tool's own error text ("Operation not
/// permitted") with no mention of the sandbox — the agent is left
/// guessing why a write outside the workspace failed, or worse,
/// re-tries with sudo. When the merged output matches a known EPERM
/// phrasing, return a note naming the policy and the alternative.
/// Coreutils/git/cargo phrasings on macOS + Linux; EACCES can be a real
/// fs-permission problem too, hence "likely".
pub fn denial_hint(spec: &SandboxSpec, output: &str) -> Option<String> {
    const EPERM_PHRASES: &[&str] = &[
        "Operation not permitted",
        "Permission denied",
        "Read-only file system",
    ];
    if !EPERM_PHRASES.iter().any(|p| output.contains(p)) {
        return None;
    }
    let roots: Vec<String> = spec
        .writable
        .iter()
        .map(|p| p.display().to_string())
        .chain(BUILTIN_WRITABLE.iter().map(|s| s.to_string()))
        .collect();
    Some(format!(
        "Note: this command ran sandboxed (writes confined to: {}). \
         The denial above is likely the sandbox policy rather than real file permissions. \
         For file changes outside those roots use the write/edit tools instead of shell redirection.",
        roots.join(", ")
    ))
}

/// Seatbelt profile: allow everything by default EXCEPT writes outside the
/// writable set (and optionally network). "Allow read, deny stray writes"
/// keeps compilers/package managers working while protecting the system.
pub fn seatbelt_profile(spec: &SandboxSpec) -> String {
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    for dir in &spec.writable {
        // Seatbelt matches against RESOLVED paths: a writable dir reached
        // through a symlink (e.g. /tmp → /private/tmp, or a symlinked cwd)
        // would never match its literal spelling and every write would be
        // denied — fail-closed, but breaking the command for no visible
        // reason. Canonicalize so the profile names the real path.
        let resolved = dunce::canonicalize(dir).unwrap_or_else(|_| dir.clone());
        profile.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            sbpl_escape(&resolved.display().to_string())
        ));
    }
    profile.push_str("(allow file-write* (subpath \"/tmp\") (subpath \"/private/tmp\")");
    profile.push_str(" (subpath \"/private/var/folders\") (subpath \"/dev\"))\n");
    debug_assert_eq!(BUILTIN_WRITABLE.len(), 4, "profile/list drift");
    if !spec.network {
        profile.push_str("(deny network*)\n");
    }
    profile
}

/// Bubblewrap argv prefix (the shell argv is appended after it).
pub fn bubblewrap_args(spec: &SandboxSpec, cwd: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "--die-with-parent",
        "--new-session",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // The workspace is always writable.
    let mut writable = spec.writable.clone();
    if !writable.iter().any(|d| d == cwd) {
        writable.push(cwd.to_path_buf());
    }
    for dir in writable {
        // `bwrap --bind` requires the source dir to EXIST; a declared-but-
        // missing writable dir aborts the whole launch with an opaque
        // "bwrap: Can't mkdir ... No such file or directory". Create it
        // (it's declared writable anyway) and skip it if that fails.
        if !dir.exists()
            && let Err(e) = std::fs::create_dir_all(&dir)
        {
            tracing::warn!(
                "sandbox: skipping writable dir {} (cannot create it: {e}); \
                 the command will not see it as writable",
                dir.display()
            );
            continue;
        }
        let display = dir.display().to_string();
        args.push("--bind".to_string());
        args.push(display.clone());
        args.push(display);
    }
    if !spec.network {
        args.push("--unshare-net".to_string());
    }
    args.push("--chdir".to_string());
    args.push(cwd.display().to_string());
    args.push("--".to_string());
    args
}

/// Wrap a shell invocation in the sandbox. Returns `(program, argv_prefix)`:
/// spawn `program argv_prefix… <shell> <shell args…> <command>`. None when no
/// backend is available (caller falls back to unsandboxed execution).
pub fn wrap_invocation(
    backend: &SandboxBackend,
    spec: &SandboxSpec,
    cwd: &Path,
) -> (PathBuf, Vec<String>) {
    match backend {
        SandboxBackend::Bubblewrap(bwrap) => (bwrap.clone(), bubblewrap_args(spec, cwd)),
        SandboxBackend::Seatbelt(seatbelt) => (
            seatbelt.clone(),
            vec!["-p".to_string(), seatbelt_profile(spec)],
        ),
        // Job Objects attach post-spawn; no argv wrapper.
        SandboxBackend::WindowsJob => (shell_placeholder(), Vec::new()),
    }
}

/// Unreachable placeholder for the WindowsJob branch of wrap_invocation
/// (kept total; plan() never routes WindowsJob here).
fn shell_placeholder() -> PathBuf {
    PathBuf::new()
}

/// Convenience: full sandbox-aware spawn plan for a shell command.
/// Returns `(program, args)` ready for `tokio::process::Command`.
/// (The WindowsJob backend has no argv wrapper — it attaches post-spawn.)
pub fn plan(
    backend: Option<&SandboxBackend>,
    spec: Option<&SandboxSpec>,
    shell: &ShellConfig,
    command: &str,
    cwd: &Path,
) -> (PathBuf, Vec<String>) {
    match (backend, spec) {
        (Some(SandboxBackend::Bubblewrap(_) | SandboxBackend::Seatbelt(_)), Some(spec)) => {
            let (program, mut args) = wrap_invocation(backend.expect("checked"), spec, cwd);
            args.push(shell.shell.display().to_string());
            args.extend(shell.args.iter().cloned());
            args.push(command.to_string());
            (program, args)
        }
        _ => {
            let mut args = shell.args.clone();
            args.push(command.to_string());
            (shell.shell.clone(), args)
        }
    }
}

/// Resolve a spec against the detected backend. Logs once when sandboxing
/// was requested but the platform has no backend (e.g. Windows).
pub fn resolve(spec: &SandboxSpec) -> Option<(SandboxBackend, SandboxSpec)> {
    match detect() {
        Some(backend) => Some((backend, spec.clone())),
        None => {
            static WARN_ONCE: std::sync::Once = std::sync::Once::new();
            WARN_ONCE.call_once(|| {
                tracing::warn!(
                    "sandbox requested but no OS sandbox backend is available on this \
                     platform (needs bubblewrap on Linux or sandbox-exec on macOS); \
                     bash commands run unsandboxed"
                );
            });
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            writable: vec![PathBuf::from("/work")],
            ..Default::default()
        }
    }

    #[test]
    fn seatbelt_profile_confines_writes() {
        let profile = seatbelt_profile(&spec());
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(subpath \"/work\")"));
        assert!(!profile.contains("deny network"));
        let offline = seatbelt_profile(&SandboxSpec {
            network: false,
            ..spec()
        });
        assert!(offline.contains("(deny network*)"));
    }

    #[test]
    fn writable_covers_declared_and_builtin_roots() {
        let spec = spec();
        assert!(writable_covers(
            &spec,
            std::path::Path::new("/work/sub/dir")
        ));
        assert!(writable_covers(&spec, std::path::Path::new("/tmp/stuff")));
        assert!(!writable_covers(
            &spec,
            std::path::Path::new("/usr/local/lib")
        ));
    }

    #[test]
    fn cargo_home_env_falls_back_when_home_not_writable() {
        let spec = spec();
        // Unsandboxed: no fallback.
        assert!(
            cargo_home_env_inner(None, false, Some(std::path::Path::new("/home/u/.cargo")))
                .is_none()
        );
        // Explicit CARGO_HOME wins, right or wrong.
        assert!(
            cargo_home_env_inner(Some(&spec), true, Some(std::path::Path::new("/usr/cargo")))
                .is_none()
        );
        // Default home inside a writable root: nothing to do.
        assert!(
            cargo_home_env_inner(
                Some(&spec),
                false,
                Some(std::path::Path::new("/work/.cargo"))
            )
            .is_none()
        );
        // Default home NOT writable: fall back to the temp dir.
        let (key, dir) = cargo_home_env_inner(
            Some(&spec),
            false,
            Some(std::path::Path::new("/home/u/.cargo")),
        )
        .unwrap();
        assert_eq!(key, "CARGO_HOME");
        assert!(dir.ends_with("tack-cargo-home"), "{dir:?}");
        assert!(dir.exists(), "fallback dir must be created");
    }

    #[test]
    fn denial_hint_flags_eperm_and_lists_roots() {
        let spec = spec();
        let hint = denial_hint(&spec, "cp: /etc/passwd: Operation not permitted").unwrap();
        assert!(hint.contains("/work"), "{hint}");
        assert!(hint.contains("write/edit tools"), "{hint}");
        assert!(denial_hint(&spec, "cp: /etc/passwd: Permission denied").is_some());
        assert!(denial_hint(&spec, "all good, exit 1").is_none());
    }

    #[test]
    fn seatbelt_profile_escapes_quotes_and_control_chars() {
        let spec = SandboxSpec {
            writable: vec![
                PathBuf::from("/work/with \"quote\""),
                PathBuf::from("/work/with\nnewline"),
            ],
            ..Default::default()
        };
        let profile = seatbelt_profile(&spec);
        // The quote is escaped; no raw `"` from the path can terminate the
        // string literal early.
        assert!(
            profile.contains("(subpath \"/work/with \\\"quote\\\"\")"),
            "{profile}"
        );
        // The raw newline must not survive into the profile (it would
        // inject arbitrary profile lines); only the escaped two-char form.
        assert!(profile.contains("/work/with\\nnewline"), "{profile}");
        assert!(
            !profile.contains("/work/with\nnewline"),
            "raw newline leaked"
        );
    }

    /// End-to-end against the real sandbox-exec parser: a workspace whose
    /// path contains a `"` must still be writable, with outside writes
    /// denied. (Previously the unescaped quote broke the whole profile.)
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_with_quoted_path_works_under_sandbox_exec() {
        let seatbelt = PathBuf::from("/usr/bin/sandbox-exec");
        if !seatbelt.exists() {
            return;
        }
        // sandbox_apply is denied inside an already-sandboxed process tree
        // (e.g. this test running under tack's own bash sandbox). Skip
        // there: nesting is impossible by seatbelt design, not a bug.
        let nested = std::process::Command::new(&seatbelt)
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true);
        if nested {
            eprintln!("skipping: already inside a seatbelt sandbox (nested apply denied)");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // Resolve symlinks (/tmp → /private/tmp): seatbelt matches resolved
        // paths, like callers passing the canonical cwd.
        let base = tmp.path().canonicalize().unwrap().join("with \"quote\"");
        std::fs::create_dir_all(&base).unwrap();
        let spec = SandboxSpec {
            writable: vec![base.clone()],
            ..Default::default()
        };
        let profile = seatbelt_profile(&spec);

        let inside = base.join("inside.txt");
        let status = std::process::Command::new(&seatbelt)
            .args(["-p", &profile, "/usr/bin/touch"])
            .arg(&inside)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "write inside quoted workspace must be allowed"
        );
        assert!(inside.exists());

        // "Outside" must be somewhere the profile's built-in allowances
        // (/tmp, /private/var/folders) do NOT cover — use a probe in $HOME.
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let outside = home.join(format!(".tack-sandbox-probe-{}", std::process::id()));
        let status = std::process::Command::new(&seatbelt)
            .args(["-p", &profile, "/usr/bin/touch"])
            .arg(&outside)
            .status()
            .unwrap();
        let _ = std::fs::remove_file(&outside);
        assert!(!status.success(), "write outside workspace must be denied");
        assert!(!outside.exists());
    }

    #[test]
    fn bubblewrap_args_bind_workspace_rw() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let work_spec = SandboxSpec {
            writable: vec![work.clone()],
            ..Default::default()
        };
        let args = bubblewrap_args(&work_spec, &work);
        let joined = args.join(" ");
        let w = work.display().to_string();
        assert!(joined.contains("--ro-bind / /"));
        assert!(joined.contains(&format!("--bind {w} {w}")));
        assert!(!joined.contains("--unshare-net"));
        let offline = bubblewrap_args(
            &SandboxSpec {
                network: false,
                ..work_spec
            },
            &work,
        );
        assert!(offline.join(" ").contains("--unshare-net"));
    }

    #[test]
    fn plan_without_backend_is_passthrough() {
        let shell = ShellConfig {
            shell: PathBuf::from("/bin/bash"),
            args: vec!["-c".to_string()],
            transport: crate::shell::CommandTransport::Argv,
        };
        let (program, args) = plan(None, Some(&spec()), &shell, "ls", Path::new("/work"));
        assert_eq!(program, PathBuf::from("/bin/bash"));
        assert_eq!(args, vec!["-c", "ls"]);
    }

    /// Regression: seatbelt matches resolved paths, so a writable dir
    /// behind a symlink (macOS /var → /private/var) must be canonicalized
    /// into the profile — otherwise every write is denied for no visible
    /// reason. macOS-only: seatbelt is the macOS backend; elsewhere the
    /// profile string escaping differs and this test has no meaning.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_canonicalizes_writable_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = dunce::canonicalize(tmp.path()).unwrap();
        // tempdir paths under /var/folders on macOS are symlinked; skip if
        // this platform's temp dir has no symlink component.
        if canonical == tmp.path() {
            eprintln!("temp dir is not behind a symlink; nothing to test");
        }
        let spec = SandboxSpec {
            writable: vec![tmp.path().to_path_buf()],
            ..Default::default()
        };
        let profile = seatbelt_profile(&spec);
        assert!(
            profile.contains(&format!("(subpath \"{}\")", canonical.display())),
            "{profile}"
        );
    }

    /// bwrap --bind requires existing dirs: a missing writable dir must be
    /// created, and an uncreatable one skipped instead of aborting launch.
    #[test]
    fn bubblewrap_args_handles_missing_writable_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("declared-but-missing/nested");
        // A dir whose parent is a FILE can never be created.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "file, not a dir").unwrap();
        let uncreatable = blocker.join("child");

        let spec = SandboxSpec {
            writable: vec![missing.clone(), uncreatable.clone()],
            ..Default::default()
        };
        let args = bubblewrap_args(&spec, tmp.path()).join(" ");
        assert!(missing.is_dir(), "missing writable dir must be created");
        assert!(
            args.contains(&format!(
                "--bind {} {}",
                missing.display(),
                missing.display()
            )),
            "{args}"
        );
        assert!(!args.contains(&uncreatable.display().to_string()), "{args}");
    }

    #[test]
    fn plan_with_seatbelt_wraps() {
        let shell = ShellConfig {
            shell: PathBuf::from("/bin/bash"),
            args: vec!["-c".to_string()],
            transport: crate::shell::CommandTransport::Argv,
        };
        let backend = SandboxBackend::Seatbelt(PathBuf::from("/usr/bin/sandbox-exec"));
        let (program, args) = plan(
            Some(&backend),
            Some(&spec()),
            &shell,
            "make",
            Path::new("/work"),
        );
        assert_eq!(program, PathBuf::from("/usr/bin/sandbox-exec"));
        assert_eq!(args[0], "-p");
        assert!(args[1].contains("deny file-write*"));
        assert_eq!(args[2], "/bin/bash");
        assert_eq!(args.last().unwrap(), "make");
    }
}
