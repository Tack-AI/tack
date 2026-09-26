//! Headless-browser page rendering for web_fetch: JS-heavy pages (SPA shells)
//! that reduce to nothing under the plain HTML→text path are re-fetched with
//! a real browser's `--dump-dom` (Chrome/Edge/Chromium, auto-discovered).
//!
//! No new dependencies: the browser is a subprocess, DOM HTML comes back on
//! stdout and flows through the same html_to_text pipeline. Discovery result
//! is cached process-wide; absence of a browser degrades to the plain path.

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

/// When to use headless rendering (settings `webRender`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WebRenderMode {
    /// Render only when the plain fetch looks like a JS shell.
    #[default]
    Auto,
    /// Always render (when a browser is available).
    Always,
    /// Never render.
    Off,
}

impl WebRenderMode {
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("always") => WebRenderMode::Always,
            Some("off") => WebRenderMode::Off,
            _ => WebRenderMode::Auto,
        }
    }
}

/// Discover a headless-capable browser, cached process-wide.
/// `TACK_BROWSER` env overrides (path to the browser executable).
pub fn find_browser() -> Option<PathBuf> {
    static CACHE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHE.get_or_init(discover_browser).clone()
}

fn discover_browser() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("TACK_BROWSER") {
        let path = PathBuf::from(custom);
        return path.exists().then_some(path);
    }

    #[cfg(windows)]
    {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Ok(dir) = std::env::var(var) {
                let dir = PathBuf::from(dir);
                // Edge is ubiquitous on Windows; prefer it, then Chrome.
                candidates.push(
                    dir.join("Microsoft")
                        .join("Edge")
                        .join("Application")
                        .join("msedge.exe"),
                );
                candidates.push(
                    dir.join("Google")
                        .join("Chrome")
                        .join("Application")
                        .join("chrome.exe"),
                );
            }
        }
        for path in candidates {
            if path.exists() {
                return Some(path);
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    {
        for path in [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ] {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }
        None
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for program in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ] {
            if let Ok(output) = std::process::Command::new("which").arg(program).output()
                && output.status.success()
            {
                let first = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !first.is_empty() {
                    return Some(PathBuf::from(first));
                }
            }
        }
        None
    }
}

/// Heuristic: plain fetch produced almost nothing while the page ships lots
/// of script — an SPA shell that needs a real browser.
pub fn looks_like_js_shell(html: &str, extracted_text: &str) -> bool {
    let text_len = extracted_text.trim().len();
    if text_len >= 300 {
        return false;
    }
    let lower = html.to_lowercase();
    let script_count = lower.matches("<script").count();
    script_count >= 2
        || lower.contains("enable javascript")
        || lower.contains("__next_data__")
        || lower.contains("id=\"root\"")
        || lower.contains("id=\"app\"")
}

/// Hard cap on the dumped DOM: Chrome's stdout is otherwise unbounded and
/// a pathological page could exhaust memory. Anything larger is useless to
/// the model anyway (the text pipeline truncates far earlier).
const MAX_DOM_BYTES: usize = 16 * 1024 * 1024;

/// Read an async stream into memory, stopping at `cap` bytes. Returns the
/// bytes and whether the cap cut the stream short.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
    cancel: &CancellationToken,
) -> Result<(Vec<u8>, bool), String> {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
            read = reader.read(&mut chunk) => {
                match read {
                    Ok(0) => return Ok((buf, false)),
                    Ok(n) => {
                        let remaining = cap.saturating_sub(buf.len());
                        if n >= remaining {
                            buf.extend_from_slice(&chunk[..remaining]);
                            return Ok((buf, true));
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    Err(e) => return Err(format!("browser stdout read failed: {e}")),
                }
            }
        }
    }
}

/// Render `url` in headless mode and return the post-JS DOM as HTML.
///
/// SSRF note: the caller validates `url` with web::ensure_public_url, but
/// the browser follows any redirects (and subresource loads) on its own —
/// `--dump-dom` offers no per-request hook to re-validate each hop the way
/// the plain fetch path does, and the final post-redirect URL is not
/// reported. This residual gap is accepted for the render path (opt-in /
/// auto-fallback for JS shells); the plain fetch path validates every hop.
pub async fn dump_dom(url: &str, cancel: &CancellationToken) -> Result<String, String> {
    let browser = find_browser().ok_or_else(|| {
        "no headless browser found (install Chrome/Edge or set TACK_BROWSER)".to_string()
    })?;
    // Isolated throwaway profile: without --user-data-dir Chrome reuses the
    // user's real profile (cookies, sessions, extensions) for fetches made
    // by the agent, and concurrent runs can wedge each other's profile lock.
    let profile_dir = std::env::temp_dir().join(format!(
        "tack-chrome-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&profile_dir)
        .map_err(|e| format!("cannot create browser profile dir: {e}"))?;
    let mut cmd = tokio::process::Command::new(browser);
    cmd.args([
        "--headless=new",
        "--disable-gpu",
        "--disable-extensions",
        "--no-first-run",
        "--mute-audio",
        "--hide-scrollbars",
        // Let JS run up to 10s of virtual time before dumping.
        "--virtual-time-budget=10000",
    ])
    .arg(format!("--user-data-dir={}", profile_dir.display()))
    .arg("--dump-dom")
    .arg(url)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        // Own process group so kill_process_tree's group kill reaches
        // chrome's renderer/gpu children — otherwise only the main process
        // dies and the helpers are orphaned on cancel/timeout.
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn browser: {e}"))?;
    let pid = child.id().unwrap_or(0);
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "browser stdout was not piped".to_string())?;
    let read = read_capped(&mut stdout, MAX_DOM_BYTES, cancel);
    tokio::pin!(read);
    let (bytes, capped) = tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(45)) => {
            crate::shell::kill_process_tree(pid);
            // Reap after the kill (same as executor's cancel/timeout
            // branches): kill without wait leaves a zombie.
            let _ = child.wait().await;
            let _ = std::fs::remove_dir_all(&profile_dir);
            return Err("headless render timed out (45s)".to_string());
        }
        result = &mut read => match result {
            Ok(done) => done,
            Err(e) => {
                // Cancel or read failure: don't leave Chrome running.
                crate::shell::kill_process_tree(pid);
                let _ = child.wait().await;
                let _ = std::fs::remove_dir_all(&profile_dir);
                return Err(e);
            }
        },
    };
    if capped {
        // The DOM is far beyond usable; stop Chrome (it blocks on the pipe
        // once we stop reading) and keep what arrived.
        crate::shell::kill_process_tree(pid);
    }
    let _ = child.wait().await;
    let _ = std::fs::remove_dir_all(&profile_dir);
    let html = String::from_utf8_lossy(&bytes).to_string();
    if html.trim().is_empty() {
        return Err("headless render produced no DOM".to_string());
    }
    Ok(html)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn js_shell_heuristic() {
        let spa = "<html><body><div id=\"root\"></div><script src=a.js></script><script src=b.js></script></body></html>";
        assert!(looks_like_js_shell(spa, ""));
        let article =
            "<html><body><p>lots of real content here that goes on and on</p></body></html>";
        assert!(!looks_like_js_shell(article, &"real content ".repeat(40)));
        // Short text but no scripts: just a short page, not an SPA shell.
        assert!(!looks_like_js_shell("<p>hi</p>", "hi"));
    }

    /// Chrome's stdout is capped: a stream larger than the cap is cut
    /// short (and the caller kills the browser), not buffered unboundedly.
    #[tokio::test]
    async fn read_capped_bounds_output() {
        let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = tx.write_all(&vec![b'x'; 10_000]).await;
            // Keep the stream OPEN: the cap (not EOF) must end the read.
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        let (bytes, capped) = read_capped(&mut rx, 1000, &CancellationToken::new())
            .await
            .unwrap();
        assert!(capped);
        assert_eq!(bytes.len(), 1000);

        // Under the cap: full content, EOF-terminated.
        let (mut tx, mut rx) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = tx.write_all(b"hello").await;
        });
        let (bytes, capped) = read_capped(&mut rx, 1000, &CancellationToken::new())
            .await
            .unwrap();
        assert!(!capped);
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn render_mode_parsing() {
        assert_eq!(
            WebRenderMode::from_setting(Some("always")),
            WebRenderMode::Always
        );
        assert_eq!(WebRenderMode::from_setting(Some("off")), WebRenderMode::Off);
        assert_eq!(WebRenderMode::from_setting(None), WebRenderMode::Auto);
        assert_eq!(
            WebRenderMode::from_setting(Some("bogus")),
            WebRenderMode::Auto
        );
    }

    /// Live smoke test against a real browser. Run with:
    /// `cargo test -p tack-tools --lib browser -- --ignored`
    #[tokio::test]
    #[ignore = "requires Chrome/Edge and network"]
    async fn dump_dom_live() {
        let Some(browser) = find_browser() else {
            eprintln!("no browser installed; skipping");
            return;
        };
        eprintln!("browser: {}", browser.display());
        let html = dump_dom("https://example.com/", &CancellationToken::new())
            .await
            .unwrap();
        assert!(html.to_lowercase().contains("example domain"), "{html}");
    }
}
