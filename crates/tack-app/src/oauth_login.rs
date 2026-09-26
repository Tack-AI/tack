//! Interactive OAuth login: loopback callback server, browser open, manual
//! paste fallback, device-code display. Port of TS pi's login-dialog flow
//! (`cli`-side glue for `tack-ai::oauth`).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tack_ai::oauth::{BrowserFlow, OAuthCredential, OAuthFlow, oauth_flow};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Best-effort browser open (TS `open-browser.ts`): shell-free, detached.
pub fn open_browser(url: &str) {
    let result = std::process::Command::new(browser_command().0)
        .args(browser_command().1)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match result {
        Ok(mut child) => {
            // Reap the launcher on a background thread: xdg-open/open exit
            // quickly and would otherwise linger as a zombie until the
            // whole process exits. (Not tokio::process: open_browser is a
            // sync API also called outside a runtime context.)
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::debug!("failed to open browser: {e}"),
    }
}

fn browser_command() -> (&'static str, Vec<&'static str>) {
    #[cfg(target_os = "windows")]
    {
        ("rundll32", vec!["url.dll,FileProtocolHandler"])
    }
    #[cfg(target_os = "macos")]
    {
        ("open", vec![])
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        ("xdg-open", vec![])
    }
}

/// Parse a manual paste into an authorization code. Accepts a full redirect
/// URL, a `code#state` pair, a query string, or a bare code (TS
/// `parseManualInput`). Returns (code, optional state).
pub fn parse_manual_input(input: &str) -> Option<(String, Option<String>)> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    // Full URL or query string.
    if input.contains("code=") {
        let query = input
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or(input)
            .split('#')
            .next()
            .unwrap_or(input);
        let mut code = None;
        let mut state = None;
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                match k {
                    "code" => code = Some(v.to_string()),
                    "state" => state = Some(v.to_string()),
                    _ => {}
                }
            }
        }
        return code.map(|c| (c, state));
    }
    // code#state
    if let Some((code, state)) = input.split_once('#') {
        return Some((code.to_string(), Some(state.to_string())));
    }
    Some((input.to_string(), None))
}

/// One-shot loopback callback server: accepts a single GET, extracts
/// `code`/`state` from the query, replies with a closable HTML page.
pub async fn await_callback(
    listener: tokio::net::TcpListener,
    path: &str,
    expected_state: &str,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut socket, _) = listener
        .accept()
        .await
        .context("callback listener accept failed")?;
    let mut buf = vec![0u8; 16384];
    let n = socket
        .read(&mut buf)
        .await
        .context("callback read failed")?;
    let request = String::from_utf8_lossy(&buf[..n]).to_string();
    let request_line = request.lines().next().unwrap_or("").to_string();

    let (html, result) = match parse_callback_request(&request_line, path, expected_state) {
        Ok(code) => (
            "<html><body><h2>Login successful</h2><p>You can close this tab and return to the terminal.</p></body></html>",
            Ok(code),
        ),
        Err(e) => (
            "<html><body><h2>Login failed</h2><p>State mismatch or malformed callback; see the terminal.</p></body></html>",
            Err(e),
        ),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    result
}

/// One-shot loopback callback server: accepts a single GET, extracts
/// `code`/`state` from the query, replies with a closable HTML page.
/// Returns (code, state) for callers that need the state (MCP OAuth).
pub async fn await_callback_pair(
    listener: tokio::net::TcpListener,
    path: &str,
) -> Result<(String, Option<String>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut socket, _) = listener
        .accept()
        .await
        .context("callback listener accept failed")?;
    let mut buf = vec![0u8; 16384];
    let n = socket
        .read(&mut buf)
        .await
        .context("callback read failed")?;
    let request = String::from_utf8_lossy(&buf[..n]).to_string();
    let request_line = request.lines().next().unwrap_or("").to_string();

    let result = parse_callback_pair(&request_line, path);
    let html = if result.is_ok() {
        "<html><body><h2>Login successful</h2><p>You can close this tab and return to the terminal.</p></body></html>"
    } else {
        "<html><body><h2>Login failed</h2><p>Malformed callback; see the terminal.</p></body></html>"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    result
}

pub fn parse_callback_request(
    request_line: &str,
    path: &str,
    expected_state: &str,
) -> Result<String> {
    let (code, state) = parse_callback_pair(request_line, path)?;
    if !expected_state.is_empty() {
        let state = state.context("callback has no state")?;
        if state != expected_state {
            bail!("callback state mismatch");
        }
    }
    Ok(code)
}

/// Extract (code, state) from a callback GET without validating state
/// (callers that need the state value itself, e.g. MCP OAuth).
pub fn parse_callback_pair(request_line: &str, path: &str) -> Result<(String, Option<String>)> {
    let target = request_line
        .strip_prefix("GET ")
        .and_then(|rest| rest.rsplit_once(" HTTP/").map(|(t, _)| t))
        .context("malformed callback request")?;
    let (req_path, query) = target.split_once('?').unwrap_or((target, ""));
    if !path.is_empty() && req_path != path {
        bail!("unexpected callback path {req_path}");
    }
    let (mut code, mut state) = (None, None);
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "code" => code = Some(percent_decode(v)),
                "state" => state = Some(percent_decode(v)),
                _ => {}
            }
        }
    }
    let code = code.context("callback has no code")?;
    Ok((code, state))
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Slice bytes (not the str) so a multi-byte UTF-8 char right after
        // `%` can't trigger a char-boundary panic; non-ASCII simply isn't
        // valid hex and falls through to the literal branch.
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Read one line from stdin on a blocking thread (raced against the callback).
async fn read_manual_paste() -> Result<String> {
    let line = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line)
    })
    .await
    .context("stdin reader panicked")?
    .context("failed to read stdin")?;
    Ok(line)
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Run a device-code login: print the code/URI, poll, return the credential.
async fn run_device_flow(
    flow: &dyn OAuthFlow,
    client: &reqwest::Client,
    options: &BTreeMap<String, String>,
) -> Result<OAuthCredential> {
    let auth = flow
        .start_device(client, options)
        .await
        .map_err(|e| anyhow::anyhow!("device authorization failed: {e}"))?;
    eprintln!();
    eprintln!(
        "  Open:  {}",
        auth.verification_uri_complete
            .as_deref()
            .unwrap_or(&auth.verification_uri)
    );
    eprintln!("  Code:  {}", auth.user_code);
    eprintln!();
    eprintln!("Waiting for authorization...");
    flow.poll_device(client, &auth)
        .await
        .map_err(|e| anyhow::anyhow!("device login failed: {e}"))
}

/// Run a browser login: loopback callback raced against manual paste.
async fn run_browser_flow(
    flow: &dyn OAuthFlow,
    client: &reqwest::Client,
    mut options: BTreeMap<String, String>,
) -> Result<OAuthCredential> {
    // Bind first so ephemeral-port flows (openrouter) know their port.
    // Fixed-port flows retry without a pre-bound listener on conflict.
    let probe = flow.start_browser(client, &BTreeMap::new()).await;
    let needs_port = probe.is_err();
    let mut listener = None;
    if needs_port {
        let bound = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = bound.local_addr()?.port();
        options.insert("callback_port".to_string(), port.to_string());
        listener = Some(bound);
    }

    let Some(browser) = flow
        .start_browser(client, &options)
        .await
        .map_err(|e| anyhow::anyhow!("failed to build authorize URL: {e}"))?
    else {
        bail!("{} has no browser flow; use --device-code", flow.id());
    };

    let listener = match listener {
        Some(l) => l,
        None => match tokio::net::TcpListener::bind(("127.0.0.1", browser.callback_port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "warning: cannot bind callback port {} ({e}); manual paste only",
                    browser.callback_port
                );
                return manual_exchange(flow, client, &browser).await;
            }
        },
    };

    eprintln!();
    eprintln!("  {}", browser.authorize_url);
    eprintln!();
    open_browser(&browser.authorize_url);
    eprintln!("Browser opened. If it didn't, open the URL above.");
    eprintln!("Or paste the redirect URL / code here and press Enter:");

    let callback = await_callback(listener, &browser.callback_path, &browser.state);
    let paste = read_manual_paste();
    let code = tokio::select! {
        result = tokio::time::timeout(CALLBACK_TIMEOUT, callback) => {
            result.context("login timed out")??
        }
        pasted = paste => {
            let pasted = pasted?;
            let (code, state) = parse_manual_input(&pasted)
                .context("could not parse pasted input")?;
            if !browser.state.is_empty()
                && let Some(state) = state
                    && state != browser.state {
                        bail!("pasted state mismatch");
                    }
            code
        }
    };
    flow.exchange_code(client, &browser, &code)
        .await
        .map_err(|e| anyhow::anyhow!("code exchange failed: {e}"))
}

async fn manual_exchange(
    flow: &dyn OAuthFlow,
    client: &reqwest::Client,
    browser: &BrowserFlow,
) -> Result<OAuthCredential> {
    eprintln!();
    eprintln!("  {}", browser.authorize_url);
    eprintln!();
    open_browser(&browser.authorize_url);
    eprintln!("Open the URL above, authorize, then paste the redirect URL / code:");
    let pasted = read_manual_paste().await?;
    let (code, state) = parse_manual_input(&pasted).context("could not parse pasted input")?;
    if !browser.state.is_empty()
        && let Some(state) = state
        && state != browser.state
    {
        bail!("pasted state mismatch");
    }
    flow.exchange_code(client, browser, &code)
        .await
        .map_err(|e| anyhow::anyhow!("code exchange failed: {e}"))
}

/// `tack login --provider <p>` OAuth entry point. Stores the credential in
/// auth.json on success. Device-only providers use the device flow even
/// without `--device-code`.
pub async fn run_oauth_login(agent_dir: &Path, provider: &str, device_code: bool) -> Result<()> {
    let flow =
        oauth_flow(provider).with_context(|| format!("provider {provider:?} has no OAuth flow"))?;
    let client = reqwest::Client::new();

    let mut device_options = BTreeMap::new();
    if provider == "github-copilot" {
        let domain = prompt_line("GitHub Enterprise domain (blank for github.com): ")?;
        if !domain.is_empty() {
            device_options.insert("domain".to_string(), domain);
        }
    }

    let credential = if device_code {
        run_device_flow(flow, &client, &device_options).await?
    } else {
        match run_browser_flow(flow, &client, BTreeMap::new()).await {
            Ok(credential) => credential,
            Err(e) if e.to_string().contains("has no browser flow") => {
                run_device_flow(flow, &client, &device_options).await?
            }
            Err(e) => return Err(e),
        }
    };

    crate::auth::set_oauth(agent_dir, provider, &credential)
        .context("failed to store credential")?;
    eprintln!("Logged in to {provider} (OAuth).");
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::percent_decode;

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%41%42"), "AB");
    }

    /// A multi-byte UTF-8 char right after `%` must not panic on the
    /// char-boundary check; invalid hex is left literal.
    #[test]
    fn percent_decode_multibyte_after_percent() {
        assert_eq!(percent_decode("%aé"), "%aé");
        assert_eq!(percent_decode("%é0"), "%é0");
        assert_eq!(percent_decode("code=%e4%b8%ad%e6%96%87"), "code=中文");
        // Truncated sequences at the end stay literal.
        assert_eq!(percent_decode("x%a"), "x%a");
        assert_eq!(percent_decode("x%"), "x%");
    }
}
