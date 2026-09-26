//! Clipboard write path: verified native backends first, OSC 52 only where
//! the terminal is genuinely the clipboard route. Port of upstream pi
//! `utils/clipboard.ts` + `utils/wsl.ts` after the three-fix stack
//! (`3349e1db1` reject unverified local writes, `60e7e76bd` surface backend
//! failures, `6dff740fa` restore the OSC 52 fallback for headless sessions).
//!
//! Decision summary (see `write_steps` / `osc52_policy`):
//! - macOS: `pbcopy`.
//! - Windows: PowerShell `Set-Clipboard` reading a staged UTF-8 file
//!   (`clip.exe` mangles non-ASCII through the console code page), then
//!   `clip` as a last resort.
//! - Linux: `termux-clipboard-set` (Termux), `wl-copy` (Wayland),
//!   `xclip`/`xsel` (X11), in that order.
//! - WSL without a working Linux clipboard: Windows clipboard via
//!   `wslpath` + PowerShell; under Windows Terminal (`WT_SESSION`) OSC 52
//!   is tried first because it is known to work there.
//! - OSC 52 is unverifiable, so a desktop session with a display reports
//!   the failure instead of claiming success (#9618). It is still emitted
//!   for SSH/mosh sessions (reaches the client clipboard) and as the last
//!   resort on display-less Linux (containers, WSL without WSLg — #9688).

use std::fmt;
use std::io::{Read, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Encoded OSC 52 payload cap (upstream `MAX_OSC52_ENCODED_LENGTH`).
const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;
/// Timeout for clipboard writer/reader commands (upstream 5s).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for the WSL `wslpath` path conversion (upstream 1s).
const WSLPATH_TIMEOUT: Duration = Duration::from_secs(1);

/// Clipboard copy failed; the message carries platform-specific recovery
/// hints (upstream `60e7e76bd`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardError {
    message: String,
}

impl ClipboardError {
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClipboardError {}

/// OS family the copy path dispatches on. Tests pass an explicit value;
/// production uses [`ClipPlatform::current`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipPlatform {
    Mac,
    Windows,
    Linux,
    /// FreeBSD etc.: upstream routes every non-darwin/non-win32 platform
    /// through the Linux env-based tool selection, but the OSC 52 headless
    /// fallback stays Linux-only.
    OtherUnix,
}

impl ClipPlatform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::OtherUnix
        }
    }
}

/// Session environment snapshot the backend selection runs on. Pure data so
/// the selection logic is unit-testable; [`ClipboardEnv::detect`] is the
/// only impure part.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClipboardEnv {
    /// `TERMUX_VERSION` — Android Termux with the Termux:API addon.
    pub termux: bool,
    /// `WAYLAND_DISPLAY` set.
    pub wayland: bool,
    /// `DISPLAY` set (X11 / XWayland / WSLg).
    pub x11: bool,
    /// SSH or mosh session (`SSH_CONNECTION` / `SSH_CLIENT` /
    /// `MOSH_CONNECTION`): the clipboard that matters lives on the client.
    pub remote: bool,
    /// Windows Subsystem for Linux (see `detect_wsl`).
    pub wsl: bool,
    /// `WT_SESSION` — running inside Windows Terminal, where OSC 52 works.
    pub windows_terminal: bool,
}

impl ClipboardEnv {
    pub fn detect() -> Self {
        fn set(var: &str) -> bool {
            // Mirror upstream Boolean(env.X): an empty value is falsy.
            std::env::var_os(var).is_some_and(|v| !v.is_empty())
        }
        Self {
            termux: set("TERMUX_VERSION"),
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
            remote: set("SSH_CONNECTION") || set("SSH_CLIENT") || set("MOSH_CONNECTION"),
            wsl: detect_wsl(),
            windows_terminal: set("WT_SESSION"),
        }
    }

    /// Linux with no display server and no Termux:API: containers, WSL
    /// without WSLg. The terminal is the only clipboard route there.
    fn headless_linux(self, platform: ClipPlatform) -> bool {
        platform == ClipPlatform::Linux && !self.x11 && !self.wayland && !self.termux
    }
}

/// WSL detection (upstream `utils/wsl.ts`): the interop env vars, then the
/// kernel release string. Empty values are falsy (upstream `Boolean(env.X)`).
fn detect_wsl() -> bool {
    let set = |var: &str| std::env::var_os(var).is_some_and(|v| !v.is_empty());
    if set("WSL_DISTRO_NAME") || set("WSLENV") {
        return true;
    }
    std::fs::read_to_string("/proc/version")
        .map(|release| {
            let release = release.to_lowercase();
            release.contains("microsoft") || release.contains("wsl")
        })
        .unwrap_or(false)
}

/// One direct-write attempt, tried in order until one succeeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteStep {
    /// Run `program args...` with the text piped to stdin.
    StdinCommand(&'static str, &'static [&'static str]),
    /// Stage the text in a UTF-8 temp file and `Set-Clipboard` it via
    /// PowerShell. On WSL the path is converted with `wslpath` first.
    PowerShellFile { wsl: bool },
    /// Emit OSC 52 to the terminal. Fails when the payload exceeds
    /// [`MAX_OSC52_ENCODED_LENGTH`].
    Osc52,
}

/// Ordered direct-write steps for the platform/session (upstream
/// `copyToClipboard`, post-`6dff740fa`).
///
/// Unlike upstream there is no native-clipboard binding, so macOS/Windows go
/// straight to platform commands; on Windows `clip.exe` is demoted behind a
/// PowerShell file staging step because piping through the console code page
/// mangles non-ASCII UTF-8.
fn write_steps(platform: ClipPlatform, env: ClipboardEnv) -> Vec<WriteStep> {
    let mut steps = Vec::new();
    match platform {
        ClipPlatform::Mac => steps.push(WriteStep::StdinCommand("pbcopy", &[])),
        ClipPlatform::Windows => {
            steps.push(WriteStep::PowerShellFile { wsl: false });
            steps.push(WriteStep::StdinCommand("clip", &[]));
        }
        ClipPlatform::Linux | ClipPlatform::OtherUnix => {
            if env.termux {
                steps.push(WriteStep::StdinCommand("termux-clipboard-set", &[]));
            }
            if env.wayland {
                steps.push(WriteStep::StdinCommand("wl-copy", &[]));
            }
            if env.x11 {
                steps.push(WriteStep::StdinCommand(
                    "xclip",
                    &["-selection", "clipboard"],
                ));
                steps.push(WriteStep::StdinCommand("xsel", &["--clipboard", "--input"]));
            }
        }
    }
    // WSL without a working Linux clipboard writes the Windows clipboard
    // through interop. Windows Terminal supports OSC 52, which is preferred
    // over the slower PowerShell round trip (upstream #9688).
    if platform == ClipPlatform::Linux && env.wsl {
        if env.windows_terminal {
            steps.push(WriteStep::Osc52);
        }
        steps.push(WriteStep::PowerShellFile { wsl: true });
    }
    steps
}

/// When OSC 52 may be emitted after the direct-write steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Osc52Policy {
    /// Emit even after a successful direct write: in SSH/mosh sessions the
    /// "local" clipboard is on the server and only the terminal escape
    /// reaches the client clipboard.
    Remote,
    /// Emit only when every direct write failed: display-less Linux, where
    /// the terminal is the only clipboard route (#9688).
    HeadlessFallback,
    /// Desktop session with a display: an unverified OSC 52 write must not
    /// mask a real failure (#9618).
    Never,
}

fn osc52_policy(platform: ClipPlatform, env: ClipboardEnv) -> Osc52Policy {
    if env.remote {
        Osc52Policy::Remote
    } else if env.headless_linux(platform) {
        Osc52Policy::HeadlessFallback
    } else {
        Osc52Policy::Never
    }
}

/// Failure message with platform-specific recovery hints (upstream
/// `60e7e76bd` / `6dff740fa` wording).
fn unavailable_message(platform: ClipPlatform, env: ClipboardEnv, oversized: bool) -> String {
    if oversized {
        return "Clipboard unavailable: text exceeds the OSC 52 size limit".to_string();
    }
    if platform == ClipPlatform::Linux {
        if env.termux {
            return "Clipboard unavailable: install the Termux:API app and `termux-api` package"
                .to_string();
        }
        if env.wayland {
            return "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland \
                    access"
                .to_string();
        }
        if env.x11 {
            return "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access"
                .to_string();
        }
    }
    "Clipboard unavailable".to_string()
}

/// Copy `text` to the system clipboard. Returns a descriptive
/// [`ClipboardError`] when no verified backend succeeded; OSC 52 alone never
/// counts as success on a desktop session with a display.
pub fn copy_to_clipboard(text: &str) -> Result<(), ClipboardError> {
    let platform = ClipPlatform::current();
    let env = ClipboardEnv::detect();
    copy_inner(
        platform,
        env,
        text,
        &mut run_clipboard_command,
        &mut emit_osc52,
    )
}

/// Runner seam: `input: Some(text)` pipes the text to the command's stdin
/// (stdout/stderr discarded), `input: None` captures stdout. `None` means
/// the command failed, timed out, or could not be spawned.
type CommandRunner<'a> = dyn FnMut(&str, &[&str], Option<&str>, Duration) -> Option<Vec<u8>> + 'a;

/// Testable copy pipeline: the platform/env decisions are pure data, the IO
/// is injected. Mirrors upstream `copyToClipboard`.
fn copy_inner(
    platform: ClipPlatform,
    env: ClipboardEnv,
    text: &str,
    run: &mut CommandRunner<'_>,
    emit: &mut dyn FnMut(&str) -> Osc52Outcome,
) -> Result<(), ClipboardError> {
    let mut copied = false;
    let mut osc52_attempted = false;
    let mut oversized = false;
    // Direct writes precede OSC 52 so the terminal cannot race the native
    // writer. Linux tools retain clipboard selection ownership after the
    // command exits.
    for step in write_steps(platform, env) {
        let ok = match step {
            WriteStep::StdinCommand(program, args) => {
                run(program, args, Some(text), COMMAND_TIMEOUT).is_some()
            }
            WriteStep::PowerShellFile { wsl } => copy_via_powershell_file(text, wsl, run),
            WriteStep::Osc52 => {
                osc52_attempted = true;
                let outcome = emit(text);
                oversized |= outcome == Osc52Outcome::Oversized;
                outcome == Osc52Outcome::Emitted
            }
        };
        if ok {
            copied = true;
            break;
        }
    }
    let emit_now = match osc52_policy(platform, env) {
        // Remote sessions always emit — even after a successful direct
        // write — because only the escape reaches the client clipboard.
        Osc52Policy::Remote => !osc52_attempted,
        Osc52Policy::HeadlessFallback => !copied && !osc52_attempted,
        Osc52Policy::Never => false,
    };
    if emit_now {
        let outcome = emit(text);
        if outcome == Osc52Outcome::Emitted {
            copied = true;
        } else {
            oversized |= outcome == Osc52Outcome::Oversized;
        }
    }
    if copied {
        return Ok(());
    }
    Err(ClipboardError {
        message: unavailable_message(platform, env, oversized),
    })
}

/// WSL/Windows clipboard write through PowerShell (upstream
/// `copyViaWindowsClipboard`). PowerShell reads the text from a staged file
/// because `clip.exe` and PowerShell stdin decode piped bytes with the
/// console code page, which mangles non-ASCII UTF-8.
fn copy_via_powershell_file(text: &str, wsl: bool, run: &mut CommandRunner<'_>) -> bool {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("tack-clip-{}-{unique}.txt", std::process::id()));
    let result = stage_and_copy(text, wsl, &tmp, run);
    let _ = std::fs::remove_file(&tmp);
    result
}

fn stage_and_copy(
    text: &str,
    wsl: bool,
    tmp: &std::path::Path,
    run: &mut CommandRunner<'_>,
) -> bool {
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Upstream mode 0o600: the staged text may be sensitive.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let Ok(mut file) = options.open(tmp) else {
            return false;
        };
        if file.write_all(text.as_bytes()).is_err() {
            return false;
        }
    }
    let tmp_str = tmp.to_string_lossy().into_owned();
    let win_path = if wsl {
        let Some(out) = run("wslpath", &["-w", &tmp_str], None, WSLPATH_TIMEOUT) else {
            return false;
        };
        let Ok(path) = String::from_utf8(out) else {
            return false;
        };
        let path = path.trim().to_string();
        if path.is_empty() {
            return false;
        }
        path
    } else {
        tmp_str
    };
    let escaped = win_path.replace('\'', "''");
    let script = format!(
        "Set-Clipboard -Value ([System.IO.File]::ReadAllText('{escaped}', \
         [System.Text.Encoding]::UTF8))"
    );
    let powershell = if wsl { "powershell.exe" } else { "powershell" };
    run(
        powershell,
        &["-NoProfile", "-Command", &script],
        None,
        COMMAND_TIMEOUT,
    )
    .is_some()
}

/// Real command runner (upstream `runClipboardCommand`). Clipboard writers
/// can daemonize to own the selection (xclip forks), so writers get null
/// stdout/stderr: there are no output pipes for the daemon to retain and
/// `try_wait` returns as soon as the foreground process exits.
fn run_clipboard_command(
    program: &str,
    args: &[&str],
    input: Option<&str>,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if input.is_some() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    // Write stdin on a helper thread: a backend that never reads its stdin
    // blocks a large write (> pipe capacity) forever — the timeout below
    // only covers waiting for exit, while upstream arms the abort timer at
    // spawn. On timeout/error we kill the child, which breaks the pipe and
    // unblocks the writer with EPIPE.
    let writer = if let Some(text) = input {
        let mut stdin = child.stdin.take()?;
        let text = text.to_owned();
        Some(std::thread::spawn(move || {
            // A writer may exit before consuming all input.
            let _ = stdin.write_all(text.as_bytes());
        }))
    } else {
        None
    };
    // Dropping stdin closes the pipe so the writer sees EOF.
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(_) => {
                // try_wait failure must not leak the spawned process
                // (dropping a Child does not kill it).
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    if let Some(handle) = writer {
        // The writer either finished or was unblocked by the kill above
        // (EPIPE once the pipe's read end is gone). Give it a brief grace
        // period, then detach — a daemonized backend that fully consumed
        // its input before forking never holds the writer anyway.
        let join_deadline = Instant::now() + Duration::from_millis(200);
        while !handle.is_finished() && Instant::now() < join_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(handle);
    }
    let status = status?;
    if !status.success() {
        return None;
    }
    if input.is_none() {
        // Captured commands (wslpath) produce a few bytes, so reading after
        // exit cannot deadlock on a full pipe buffer.
        let mut out = Vec::new();
        child.stdout.take()?.read_to_end(&mut out).ok()?;
        return Some(out);
    }
    Some(Vec::new())
}

/// Result of an OSC 52 emit attempt. `Oversized` means the payload was
/// refused (nothing written); `IoFailed` means the terminal write itself
/// failed — the failure copy shown to the user must distinguish the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Osc52Outcome {
    Emitted,
    Oversized,
    IoFailed,
}

/// OSC 52 clipboard write. Unverifiable — the terminal may ignore it — so
/// callers must follow [`osc52_policy`]. Returns [`Osc52Outcome::Oversized`]
/// when the payload exceeds the size cap instead of emitting a sequence
/// terminals choke on.
fn emit_osc52(text: &str) -> Osc52Outcome {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
        return Osc52Outcome::Oversized;
    }
    let mut out = std::io::stdout();
    if out
        .write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())
        .and_then(|()| out.flush())
        .is_err()
    {
        return Osc52Outcome::IoFailed;
    }
    Osc52Outcome::Emitted
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::sync::{Arc, Mutex};

    const MAC: ClipPlatform = ClipPlatform::Mac;
    const WIN: ClipPlatform = ClipPlatform::Windows;
    const LINUX: ClipPlatform = ClipPlatform::Linux;

    fn env() -> ClipboardEnv {
        ClipboardEnv::default()
    }

    #[test]
    fn macos_uses_pbcopy() {
        assert_eq!(
            write_steps(MAC, env()),
            vec![WriteStep::StdinCommand("pbcopy", &[])]
        );
    }

    #[test]
    fn windows_prefers_powershell_file_staging_over_clip() {
        assert_eq!(
            write_steps(WIN, env()),
            vec![
                WriteStep::PowerShellFile { wsl: false },
                WriteStep::StdinCommand("clip", &[]),
            ]
        );
    }

    #[test]
    fn linux_tool_selection_follows_the_session_env() {
        let termux = ClipboardEnv {
            termux: true,
            ..env()
        };
        assert_eq!(
            write_steps(LINUX, termux),
            vec![WriteStep::StdinCommand("termux-clipboard-set", &[])]
        );

        let wayland_and_x11 = ClipboardEnv {
            wayland: true,
            x11: true,
            ..env()
        };
        assert_eq!(
            write_steps(LINUX, wayland_and_x11),
            vec![
                WriteStep::StdinCommand("wl-copy", &[]),
                WriteStep::StdinCommand("xclip", &["-selection", "clipboard"]),
                WriteStep::StdinCommand("xsel", &["--clipboard", "--input"]),
            ]
        );

        let x11_only = ClipboardEnv { x11: true, ..env() };
        assert_eq!(
            write_steps(LINUX, x11_only),
            vec![
                WriteStep::StdinCommand("xclip", &["-selection", "clipboard"]),
                WriteStep::StdinCommand("xsel", &["--clipboard", "--input"]),
            ]
        );

        // No display, no Termux: nothing to run locally (OSC 52 policy
        // covers this case).
        assert_eq!(write_steps(LINUX, env()), Vec::new());
    }

    #[test]
    fn wsl_appends_windows_interop_steps() {
        let wsl = ClipboardEnv { wsl: true, ..env() };
        assert_eq!(
            write_steps(LINUX, wsl),
            vec![WriteStep::PowerShellFile { wsl: true }]
        );

        // Windows Terminal: OSC 52 first, PowerShell as the fallback
        // (upstream: WT supports OSC 52 and it avoids the slow round trip).
        let wt = ClipboardEnv {
            wsl: true,
            windows_terminal: true,
            ..env()
        };
        assert_eq!(
            write_steps(LINUX, wt),
            vec![WriteStep::Osc52, WriteStep::PowerShellFile { wsl: true }]
        );

        // WSLg: Linux tools first, Windows interop only if they fail.
        let wslg = ClipboardEnv {
            wsl: true,
            wayland: true,
            ..env()
        };
        assert_eq!(
            write_steps(LINUX, wslg),
            vec![
                WriteStep::StdinCommand("wl-copy", &[]),
                WriteStep::PowerShellFile { wsl: true },
            ]
        );
    }

    #[test]
    fn osc52_policy_gates_unverified_writes() {
        let desktop = ClipboardEnv { x11: true, ..env() };
        assert_eq!(osc52_policy(LINUX, desktop), Osc52Policy::Never);
        assert_eq!(osc52_policy(MAC, env()), Osc52Policy::Never);

        // Termux without a display is not "headless": OSC 52 would bypass
        // the Termux:API clipboard rather than reach it.
        let termux = ClipboardEnv {
            termux: true,
            ..env()
        };
        assert_eq!(osc52_policy(LINUX, termux), Osc52Policy::Never);

        // Containers / WSL without WSLg: the terminal is the only route.
        assert_eq!(osc52_policy(LINUX, env()), Osc52Policy::HeadlessFallback);

        let remote_desktop = ClipboardEnv {
            x11: true,
            remote: true,
            ..env()
        };
        assert_eq!(osc52_policy(LINUX, remote_desktop), Osc52Policy::Remote);
        assert_eq!(osc52_policy(MAC, remote_desktop), Osc52Policy::Remote);
    }

    #[test]
    fn failure_messages_carry_platform_hints() {
        let termux = ClipboardEnv {
            termux: true,
            ..env()
        };
        assert_eq!(
            unavailable_message(LINUX, termux, false),
            "Clipboard unavailable: install the Termux:API app and `termux-api` package"
        );
        let wayland = ClipboardEnv {
            wayland: true,
            x11: true,
            ..env()
        };
        assert_eq!(
            unavailable_message(LINUX, wayland, false),
            "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access"
        );
        let x11 = ClipboardEnv { x11: true, ..env() };
        assert_eq!(
            unavailable_message(LINUX, x11, false),
            "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access"
        );
        assert_eq!(
            unavailable_message(LINUX, env(), true),
            "Clipboard unavailable: text exceeds the OSC 52 size limit"
        );
        assert_eq!(
            unavailable_message(MAC, env(), false),
            "Clipboard unavailable"
        );
        assert_eq!(
            unavailable_message(LINUX, env(), false),
            "Clipboard unavailable"
        );
    }

    /// Recorded command invocation for the mock runner.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Call {
        program: String,
        args: Vec<String>,
        input: Option<String>,
    }

    struct Mock {
        calls: Arc<Mutex<Vec<Call>>>,
        /// Commands that "succeed"; everything else fails.
        succeed_on: Vec<String>,
        emitted: Arc<Mutex<Vec<String>>>,
        emit_outcome: Osc52Outcome,
    }

    impl Mock {
        fn new(succeed_on: &[&str]) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                succeed_on: succeed_on.iter().map(|s| s.to_string()).collect(),
                emitted: Arc::new(Mutex::new(Vec::new())),
                emit_outcome: Osc52Outcome::Emitted,
            }
        }

        fn runner(&self) -> impl FnMut(&str, &[&str], Option<&str>, Duration) -> Option<Vec<u8>> {
            let calls = self.calls.clone();
            let succeed_on = self.succeed_on.clone();
            move |program, args, input, _timeout| {
                calls.lock().unwrap().push(Call {
                    program: program.to_string(),
                    args: args.iter().map(|s| s.to_string()).collect(),
                    input: input.map(str::to_string),
                });
                if succeed_on.iter().any(|s| s == program) {
                    if program == "wslpath" {
                        Some(b"\\\\wsl.localhost\\Ubuntu\\tmp\\clip.txt\n".to_vec())
                    } else {
                        Some(Vec::new())
                    }
                } else {
                    None
                }
            }
        }

        fn emitter(&self) -> impl FnMut(&str) -> Osc52Outcome {
            let emitted = self.emitted.clone();
            let outcome = self.emit_outcome;
            move |text| {
                if outcome == Osc52Outcome::Emitted {
                    emitted.lock().unwrap().push(text.to_string());
                }
                outcome
            }
        }

        fn programs(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.program.clone())
                .collect()
        }

        fn emissions(&self) -> Vec<String> {
            self.emitted.lock().unwrap().clone()
        }
    }

    #[test]
    fn local_desktop_failure_is_an_error_not_a_silent_osc52() {
        // Regression test for upstream #9618: an ignored OSC 52 write must
        // not masquerade as success on a desktop session.
        let mock = Mock::new(&[]);
        let desktop = ClipboardEnv { x11: true, ..env() };
        let result = copy_inner(
            LINUX,
            desktop,
            "hello",
            &mut mock.runner(),
            &mut mock.emitter(),
        );
        assert_eq!(
            result.unwrap_err().message(),
            "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access"
        );
        assert_eq!(mock.programs(), vec!["xclip", "xsel"]);
        assert!(mock.emissions().is_empty());
    }

    #[test]
    fn wayland_failure_reports_wl_copy_not_the_x11_fallback() {
        let mock = Mock::new(&[]);
        let wayland = ClipboardEnv {
            wayland: true,
            x11: true,
            ..env()
        };
        let result = copy_inner(
            LINUX,
            wayland,
            "hello",
            &mut mock.runner(),
            &mut mock.emitter(),
        );
        assert_eq!(
            result.unwrap_err().message(),
            "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access"
        );
        assert_eq!(mock.programs(), vec!["wl-copy", "xclip", "xsel"]);
        assert!(mock.emissions().is_empty());
    }

    #[test]
    fn first_working_command_wins() {
        let mock = Mock::new(&["xsel"]);
        let wayland = ClipboardEnv {
            wayland: true,
            x11: true,
            ..env()
        };
        copy_inner(
            LINUX,
            wayland,
            "hello",
            &mut mock.runner(),
            &mut mock.emitter(),
        )
        .unwrap();
        assert_eq!(mock.programs(), vec!["wl-copy", "xclip", "xsel"]);
        assert!(mock.emissions().is_empty());
    }

    #[test]
    fn headless_linux_falls_back_to_osc52() {
        // Regression test for upstream #9688: containers without X11/Wayland
        // access have no other clipboard route.
        let mock = Mock::new(&[]);
        copy_inner(
            LINUX,
            env(),
            "hello",
            &mut mock.runner(),
            &mut mock.emitter(),
        )
        .unwrap();
        assert!(mock.programs().is_empty());
        assert_eq!(mock.emissions(), vec!["hello"]);
    }

    #[test]
    fn remote_session_emits_osc52_even_after_a_successful_write() {
        let mock = Mock::new(&["pbcopy"]);
        let remote = ClipboardEnv {
            remote: true,
            ..env()
        };
        copy_inner(
            MAC,
            remote,
            "hello",
            &mut mock.runner(),
            &mut mock.emitter(),
        )
        .unwrap();
        assert_eq!(mock.programs(), vec!["pbcopy"]);
        assert_eq!(mock.emissions(), vec!["hello"]);
    }

    #[test]
    fn remote_failure_uses_osc52_as_the_last_route() {
        let mock = Mock::new(&[]);
        let remote = ClipboardEnv {
            remote: true,
            ..env()
        };
        copy_inner(
            MAC,
            remote,
            "hello",
            &mut mock.runner(),
            &mut mock.emitter(),
        )
        .unwrap();
        assert_eq!(mock.programs(), vec!["pbcopy"]);
        assert_eq!(mock.emissions(), vec!["hello"]);
    }

    #[test]
    fn oversized_payload_is_an_error_not_a_truncated_write() {
        let mut mock = Mock::new(&[]);
        mock.emit_outcome = Osc52Outcome::Oversized;
        let remote = ClipboardEnv {
            remote: true,
            ..env()
        };
        let result = copy_inner(
            MAC,
            remote,
            &"x".repeat(80_000),
            &mut mock.runner(),
            &mut mock.emitter(),
        );
        assert_eq!(
            result.unwrap_err().message(),
            "Clipboard unavailable: text exceeds the OSC 52 size limit"
        );
        assert!(mock.emissions().is_empty());
    }

    #[test]
    fn wsl_windows_terminal_prefers_osc52_over_powershell() {
        let mock = Mock::new(&[]);
        let wt = ClipboardEnv {
            wsl: true,
            windows_terminal: true,
            ..env()
        };
        copy_inner(LINUX, wt, "hello", &mut mock.runner(), &mut mock.emitter()).unwrap();
        assert!(mock.programs().is_empty());
        assert_eq!(mock.emissions(), vec!["hello"]);
    }

    #[test]
    fn wsl_windows_terminal_emits_osc52_once_in_a_remote_session() {
        let mock = Mock::new(&[]);
        let wt_remote = ClipboardEnv {
            wsl: true,
            windows_terminal: true,
            remote: true,
            ..env()
        };
        copy_inner(
            LINUX,
            wt_remote,
            "hello",
            &mut mock.runner(),
            &mut mock.emitter(),
        )
        .unwrap();
        assert!(mock.programs().is_empty());
        assert_eq!(mock.emissions(), vec!["hello"]);
    }

    #[test]
    fn wsl_without_display_writes_the_windows_clipboard_via_powershell() {
        // Regression test for upstream #9688: WSL with WSLg disabled.
        let mock = Mock::new(&["wslpath", "powershell.exe"]);
        let wsl = ClipboardEnv { wsl: true, ..env() };
        let calls = mock.calls.clone();
        copy_inner(LINUX, wsl, "héllo", &mut mock.runner(), &mut mock.emitter()).unwrap();
        assert_eq!(mock.programs(), vec!["wslpath", "powershell.exe"]);
        let calls = calls.lock().unwrap();
        // wslpath received a real staged file that was cleaned up after.
        let staged = std::path::PathBuf::from(&calls[0].args[1]);
        assert!(!staged.exists());
        // PowerShell reads the Windows path back as UTF-8.
        let script = &calls[1].args[2];
        assert!(script.contains("Set-Clipboard"));
        assert!(script.contains("'\\\\wsl.localhost\\Ubuntu\\tmp\\clip.txt'"));
        assert!(mock.emissions().is_empty());
    }

    #[test]
    fn wsl_falls_back_to_osc52_when_interop_is_unavailable() {
        let mock = Mock::new(&[]);
        let wsl = ClipboardEnv { wsl: true, ..env() };
        copy_inner(LINUX, wsl, "hello", &mut mock.runner(), &mut mock.emitter()).unwrap();
        // Headless WSL: wslpath fails, PowerShell never runs, OSC 52 saves
        // the copy.
        assert_eq!(mock.programs(), vec!["wslpath"]);
        assert_eq!(mock.emissions(), vec!["hello"]);
    }

    #[test]
    fn wsl_with_a_display_prefers_linux_clipboard_tools() {
        let mock = Mock::new(&["wl-copy"]);
        let wslg = ClipboardEnv {
            wsl: true,
            wayland: true,
            ..env()
        };
        copy_inner(
            LINUX,
            wslg,
            "hello",
            &mut mock.runner(),
            &mut mock.emitter(),
        )
        .unwrap();
        assert_eq!(mock.programs(), vec!["wl-copy"]);
        assert!(mock.emissions().is_empty());
    }

    #[test]
    fn osc52_size_cap_counts_the_encoded_payload() {
        use base64::Engine;
        // 75_000 bytes encode to exactly 100_000 base64 chars: allowed.
        let at_cap = "x".repeat(75_000);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .encode(at_cap.as_bytes())
                .len(),
            MAX_OSC52_ENCODED_LENGTH
        );
        // One byte more exceeds the cap; emit_osc52 refuses without writing.
        assert_eq!(emit_osc52(&"x".repeat(75_001)), Osc52Outcome::Oversized);
    }
}

#[cfg(all(test, unix))]
mod exec_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Regression: the stdin-write phase must be covered by the timeout — a
    /// backend that never reads stdin must not block a large write past the
    /// deadline (the pre-fix code hung in `write_all` forever).
    #[test]
    fn writer_that_never_reads_stdin_is_killed_at_the_deadline() {
        let huge = "x".repeat(1 << 20); // 1 MiB, far beyond the 64 KiB pipe buffer
        let start = Instant::now();
        let out = run_clipboard_command("sleep", &["30"], Some(&huge), Duration::from_millis(300));
        assert!(out.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "took {:?} — the stdin write is not timeout-bounded",
            start.elapsed()
        );
    }

    /// Sanity: a command that consumes stdin succeeds within the timeout.
    #[test]
    fn writer_that_consumes_stdin_succeeds() {
        let out = run_clipboard_command("cat", &[], Some("hello"), Duration::from_secs(2));
        assert!(out.is_some());
    }
}
