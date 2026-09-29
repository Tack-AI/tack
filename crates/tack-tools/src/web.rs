//! Web access tools: `web_fetch` (URL → text) and `web_search`
//! (DuckDuckGo/Bing HTML scrape → result list, or a keyed API backend).
//! Read-only network access; HTML is reduced to text with a small
//! tag-stripping pass (script/style dropped, block elements become
//! newlines).

use serde_json::{Value, json};
use tack_agent_core::{AgentTool, AgentToolResult};
use tokio_util::sync::CancellationToken;

use crate::services::ToolServices;

const MAX_RESPONSE_CHARS: usize = 20_000;
const MAX_SEARCH_RESULTS: usize = 10;
/// Hard cap on downloaded body bytes: the output is truncated to
/// MAX_RESPONSE_CHARS anyway, so an unbounded read only risks memory.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

fn client_builder(redirect: reqwest::redirect::Policy) -> reqwest::ClientBuilder {
    tack_ai::tls::ensure_ring_provider();
    reqwest::Client::builder()
        .user_agent(concat!("tack/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .redirect(redirect)
}

fn build_client(redirect: reqwest::redirect::Policy) -> reqwest::Client {
    client_builder(redirect)
        .build()
        // A builder failure (e.g. TLS backend init) must not panic
        // the agent; fall back to a default-configured client.
        .unwrap_or_else(|e| {
            tracing::warn!("http client builder failed ({e}); using default client");
            reqwest::Client::new()
        })
}

fn http_client() -> reqwest::Client {
    // Built once, reused: rebuilding per request re-creates the TLS config
    // and connection pool every time. Used by the web_search backends,
    // which call fixed operator-configured API endpoints (not agent-
    // supplied URLs), so reqwest's redirect following is acceptable there.
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| build_client(reqwest::redirect::Policy::limited(5)))
        .clone()
}

/// Client for web_fetch: redirects are DISABLED so each hop can be
/// re-validated against the SSRF blocklist (see get_checked). reqwest's
/// redirect policy is a synchronous closure and cannot run the async DNS
/// validation, so automatic following would let a public URL 302 straight
/// to e.g. http://169.254.169.254/latest/meta-data unchecked.
fn fetch_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| build_client(reqwest::redirect::Policy::none()))
        .clone()
}

// ---------------------------------------------------------------------------
// SSRF protection
// ---------------------------------------------------------------------------

/// True when `ip` is a loopback / link-local / private / unspecified
/// address that web_fetch must never connect to (SSRF protection).
fn ip_is_blocked(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_loopback()          // 127.0.0.0/8
                || v4.is_private()    // RFC1918: 10/8, 172.16/12, 192.168/16
                || v4.is_link_local() // 169.254.0.0/16 (cloud metadata endpoints)
                || v4.is_unspecified() // 0.0.0.0
                // CGNAT 100.64.0.0/10 (not publicly routable).
                || (octets[0] == 100 && (octets[1] & 0xC0) == 64)
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:a.b.c.d) and deprecated IPv4-compatible
            // (::a.b.c.d) forms must be checked as the IPv4 they target.
            let segments = v6.segments();
            if segments[..5] == [0; 5] && (segments[5] == 0 || segments[5] == 0xffff) {
                let low = ((segments[6] as u32) << 16) | segments[7] as u32;
                // low 0/1 are :: and ::1, covered by the v6 checks below.
                if low > 1 {
                    return ip_is_blocked(&IpAddr::V4(std::net::Ipv4Addr::from(low)));
                }
            }
            v6.is_loopback()             // ::1
                || v6.is_unspecified()   // ::
                || v6.is_unique_local()  // fc00::/7 (IPv6 private)
                || v6.is_unicast_link_local() // fe80::/10
        }
    }
}

/// Result of the SSRF validation for one URL hop. For hostname URLs,
/// `addrs` holds the exact DNS answers that passed the blocklist; the
/// request then pins them (ClientBuilder::resolve_to_addrs) so a
/// DNS-rebinding attacker cannot return different addresses when the
/// HTTP stack resolves the name again (check/connect TOCTOU).
/// Literal-IP URLs carry no host: the stack connects to the literal
/// address, so there is nothing to re-resolve.
struct ValidatedTarget {
    host: Option<String>,
    addrs: Vec<std::net::SocketAddr>,
}

/// Validate that `url` does not target a private/local address: literal IPs
/// are checked directly; hostnames are DNS-resolved once and every returned
/// address is checked. Public URLs pass through unchanged.
async fn ensure_public_url(url: &str) -> Result<(), String> {
    validate_public_url(url).await.map(|_| ())
}

/// [`ensure_public_url`] plus the validated DNS answers, so the caller can
/// pin them on the request (anti-rebinding).
async fn validate_public_url(url: &str) -> Result<ValidatedTarget, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid url {url}: {e}"))?;
    let host_raw = parsed
        .host_str()
        .ok_or_else(|| format!("url has no host: {url}"))?;
    // IPv6 literals come back bracketed ("[::1]").
    let host = host_raw
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host_raw);
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip_is_blocked(&ip) {
            return Err(format!(
                "refusing to fetch {url}: private/local addresses are not allowed"
            ));
        }
        return Ok(ValidatedTarget {
            host: None,
            addrs: Vec::new(),
        });
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addresses: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?
        .collect();
    if addresses.is_empty() {
        return Err(format!("DNS resolution returned no addresses for {host}"));
    }
    for address in &addresses {
        if ip_is_blocked(&address.ip()) {
            return Err(format!(
                "refusing to fetch {url}: {host} resolves to private/local address {}",
                address.ip()
            ));
        }
    }
    Ok(ValidatedTarget {
        host: Some(host.to_string()),
        addrs: addresses,
    })
}

/// Maximum redirect hops web_fetch will follow (mirrors the previous
/// reqwest `Policy::limited(5)` budget).
const MAX_REDIRECTS: u32 = 5;

/// Resolve a redirect `location` header against the URL that produced it.
/// Redirect targets must stay http(s): anything else (file:, data:, ...) is
/// rejected before reqwest ever sees it.
fn resolve_redirect(current_url: &str, location: &str) -> Result<String, String> {
    let base =
        reqwest::Url::parse(current_url).map_err(|e| format!("invalid url {current_url}: {e}"))?;
    let next = base
        .join(location)
        .map_err(|e| format!("invalid redirect target {location:?}: {e}"))?;
    if next.scheme() != "http" && next.scheme() != "https" {
        return Err(format!("refusing redirect to non-http(s) url: {next}"));
    }
    Ok(next.to_string())
}

/// GET `url`, following redirects manually so every hop is re-validated
/// with the full SSRF check (literal-IP blocklist + async DNS resolution)
/// AND fetched with the validated DNS answers pinned: reqwest would
/// otherwise re-resolve the hostname between check and connect, reopening
/// the DNS-rebinding window. A redirect status without a Location header
/// is returned as-is for the caller to report.
async fn get_checked(url: &str, cancel: &CancellationToken) -> Result<reqwest::Response, String> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        // The critical validation: every hop (the initial URL and each
        // redirect target) is checked like this before connecting.
        let target = validate_public_url(&url).await?;
        let client = match &target.host {
            // One-shot client for this hop: same TLS/timeout config as
            // the shared fetch client, with the validated DNS answers
            // pinned (overrides are per-client, so the shared client
            // cannot carry them).
            Some(host) => client_builder(reqwest::redirect::Policy::none())
                .resolve_to_addrs(host, &target.addrs)
                .build()
                .map_err(|e| format!("http client build failed: {e}"))?,
            // Literal IP: nothing is re-resolved, the shared client is safe.
            None => fetch_client(),
        };
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
            r = client.get(&url).send() => r.map_err(|e| format!("fetch failed: {e}"))?,
        };
        if !response.status().is_redirection() {
            return Ok(response);
        }
        let Some(location) = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
        else {
            return Ok(response);
        };
        // Validated + pinned at the top of the next iteration.
        url = resolve_redirect(&url, location)?;
    }
    Err(format!("too many redirects (>{MAX_REDIRECTS}) for {url}"))
}

/// HTML → readable text: drop script/style/head, block tags → newlines,
/// strip remaining tags, collapse whitespace, decode a few entities.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_lowercase();
    let mut pos = 0usize;
    let bytes = html.as_bytes();
    let lower_bytes = lower.as_bytes();
    let mut skip_tag: Option<&str> = None;

    while pos < bytes.len() {
        if let Some(tag) = skip_tag {
            // Inside <script>/<style>: skip to the matching close tag.
            let close = format!("</{tag}>");
            match find_case_insensitive(lower_bytes, close.as_bytes(), pos) {
                Some(end) => {
                    pos = end + close.len();
                    skip_tag = None;
                }
                None => break,
            }
            continue;
        }
        match bytes[pos] {
            b'<' => {
                let Some(end) = find_byte(bytes, b'>', pos) else {
                    break;
                };
                let tag = std::str::from_utf8(&bytes[pos + 1..end]).unwrap_or("");
                let name: String = tag
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                if name == "script" || name == "style" {
                    skip_tag = Some(if name == "script" { "script" } else { "style" });
                } else if matches!(
                    name.as_str(),
                    "p" | "div"
                        | "br"
                        | "li"
                        | "ul"
                        | "ol"
                        | "tr"
                        | "table"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "blockquote"
                        | "pre"
                        | "hr"
                        | "title"
                ) {
                    out.push('\n');
                }
                pos = end + 1;
            }
            b'&' => {
                let end = find_byte(bytes, b';', pos).map(|e| e.min(pos + 10));
                let (entity, consumed) = match end {
                    Some(e) => (std::str::from_utf8(&bytes[pos + 1..e]).unwrap_or(""), e + 1),
                    None => ("", pos + 1),
                };
                let decoded: Option<String> = match entity {
                    "amp" => Some("&".to_string()),
                    "lt" => Some("<".to_string()),
                    "gt" => Some(">".to_string()),
                    "quot" => Some("\"".to_string()),
                    "nbsp" | "ensp" | "emsp" => Some(" ".to_string()),
                    "middot" => Some("\u{00b7}".to_string()),
                    "#39" | "apos" => Some("'".to_string()),
                    _ => entity
                        .strip_prefix('#')
                        .and_then(|num| num.parse::<u32>().ok())
                        .and_then(char::from_u32)
                        .map(|c| c.to_string()),
                };
                match decoded {
                    Some(text) => {
                        out.push_str(&text);
                        pos = consumed;
                    }
                    None => {
                        out.push('&');
                        pos += 1;
                    }
                }
            }
            _ => {
                let ch_len = utf8_len(bytes[pos]);
                out.push_str(
                    std::str::from_utf8(&bytes[pos..(pos + ch_len).min(bytes.len())]).unwrap_or(""),
                );
                pos += ch_len;
            }
        }
    }

    // Collapse whitespace: single spaces per line, blank lines deduped.
    let mut lines: Vec<String> = Vec::new();
    for line in out.lines() {
        let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            if lines.last().is_some_and(|l: &String| !l.is_empty()) {
                lines.push(String::new());
            }
        } else {
            lines.push(collapsed);
        }
    }
    lines.join("\n").trim().to_string()
}

fn find_byte(haystack: &[u8], needle: u8, from: usize) -> Option<usize> {
    haystack[from.min(haystack.len())..]
        .iter()
        .position(|&b| b == needle)
        .map(|i| from + i)
}

fn find_case_insensitive(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack[from.min(haystack.len())..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| from + i)
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

fn truncate_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max).collect();
    format!("{truncated}\n\n[truncated to {max} chars]")
}

/// Read a response body with a hard byte cap (streamed, so a hostile or
/// buggy server cannot exhaust memory). Decoded as UTF-8 lossily.
async fn read_body_capped(response: reqwest::Response, max: usize) -> Result<String, String> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read body failed: {e}"))?;
        let remaining = max.saturating_sub(buf.len());
        if chunk.len() >= remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

async fn fetch_body(url: &str, cancel: &CancellationToken) -> Result<(String, bool), String> {
    let response = get_checked(url, cancel).await?;
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !status.is_success() {
        return Err(format!("HTTP {status} for {url}"));
    }
    if !content_type.contains("html")
        && !content_type.contains("text")
        && !content_type.contains("json")
        && !content_type.is_empty()
    {
        return Err(format!(
            "unsupported content type {content_type} (only text/html, text/plain, JSON)"
        ));
    }
    let body = read_body_capped(response, MAX_BODY_BYTES).await?;
    Ok((body, content_type.contains("html")))
}

// ---------------------------------------------------------------------------
// web_fetch
// ---------------------------------------------------------------------------

pub struct WebFetchTool {
    services: ToolServices,
}

impl WebFetchTool {
    pub fn new(services: ToolServices) -> Self {
        WebFetchTool { services }
    }
}

impl std::fmt::Debug for WebFetchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebFetchTool").finish()
    }
}

#[async_trait::async_trait]
impl AgentTool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn label(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return its content as text (HTML is converted to readable text). Read-only. \
         For JS-heavy pages (SPAs), set render=true to load the page in a headless browser first; \
         JS-shell pages are retried that way automatically when a browser is installed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to fetch (http/https)" },
                "render": { "type": "boolean", "description": "Render the page in a headless browser before extracting text (for JS-heavy SPA pages)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let url = params
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing required parameter: url".to_string())?;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!("url must start with http:// or https://: {url}"));
        }
        ensure_public_url(url).await?;
        let render_param = params.get("render").and_then(Value::as_bool);
        let mode = self.services.web_render;

        let render = |cancel: CancellationToken| async move {
            let dom = crate::browser::dump_dom(url, &cancel).await?;
            Ok::<String, String>(format!(
                "(rendered with headless browser)\n\n{}",
                html_to_text(&dom)
            ))
        };

        // Explicit request (param or settings webRender=always).
        let forced = render_param == Some(true)
            || (render_param.is_none() && mode == crate::browser::WebRenderMode::Always);
        let wrap = |text: String| {
            Ok(AgentToolResult::text(crate::services::wrap_untrusted(
                &self.services,
                &format!("web_fetch {url}"),
                truncate_text(&text, MAX_RESPONSE_CHARS),
            )))
        };
        if forced {
            return wrap(render(cancel).await?);
        }

        let (body, is_html) = fetch_body(url, &cancel).await?;
        if !is_html {
            return wrap(body);
        }
        let text = html_to_text(&body);
        // Auto fallback: SPA shell (little text, lots of script) → headless.
        let may_render = render_param.is_none() && mode != crate::browser::WebRenderMode::Off;
        if may_render
            && crate::browser::looks_like_js_shell(&body, &text)
            && crate::browser::find_browser().is_some()
        {
            match render(cancel.clone()).await {
                Ok(rendered) => {
                    return wrap(rendered);
                }
                Err(e) => {
                    tracing::debug!("headless render failed for {url}: {e}");
                }
            }
        }
        wrap(text)
    }
}

// ---------------------------------------------------------------------------
// web_search
// ---------------------------------------------------------------------------

/// Search backend selection (settings `webSearch.provider`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SearchBackend {
    /// Bing HTML scrape (default, no key needed). Free scrape backends
    /// fall back to each other on failure — the two are unreachable from
    /// different networks (e.g. duckduckgo.com is blocked in some
    /// regions), and the operator should not have to know which one
    /// works where.
    #[default]
    Bing,
    /// DuckDuckGo HTML scrape (no key needed).
    DuckDuckGo,
    Brave,
    Tavily,
    Exa,
}

impl SearchBackend {
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("duckduckgo") => SearchBackend::DuckDuckGo,
            Some("brave") => SearchBackend::Brave,
            Some("tavily") => SearchBackend::Tavily,
            Some("exa") => SearchBackend::Exa,
            _ => SearchBackend::Bing,
        }
    }

    /// Environment variable consulted for the API key when settings
    /// `webSearch.apiKey` is empty.
    pub fn api_key_env(&self) -> Option<&'static str> {
        match self {
            SearchBackend::DuckDuckGo | SearchBackend::Bing => None,
            SearchBackend::Brave => Some("BRAVE_API_KEY"),
            SearchBackend::Tavily => Some("TAVILY_API_KEY"),
            SearchBackend::Exa => Some("EXA_API_KEY"),
        }
    }
}

/// web_search configuration carried in ToolServices.
#[derive(Clone, Debug, Default)]
pub struct WebSearchConfig {
    pub backend: SearchBackend,
    pub api_key: Option<String>,
}

/// One normalized result triple (title, url, snippet).
type SearchResults = Vec<(String, String, String)>;

async fn search_brave(query: &str, api_key: &str) -> Result<SearchResults, String> {
    let response = http_client()
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query), ("count", "10")])
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("brave search failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("brave search: HTTP {}", response.status()));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("brave parse failed: {e}"))?;
    Ok(body["web"]["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .map(|r| {
                    (
                        r["title"].as_str().unwrap_or_default().to_string(),
                        r["url"].as_str().unwrap_or_default().to_string(),
                        r["description"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn search_tavily(query: &str, api_key: &str) -> Result<SearchResults, String> {
    let response = http_client()
        .post("https://api.tavily.com/search")
        .json(&json!({ "query": query, "max_results": 10, "api_key": api_key }))
        .send()
        .await
        .map_err(|e| format!("tavily search failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("tavily search: HTTP {}", response.status()));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("tavily parse failed: {e}"))?;
    Ok(body["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .map(|r| {
                    (
                        r["title"].as_str().unwrap_or_default().to_string(),
                        r["url"].as_str().unwrap_or_default().to_string(),
                        r["content"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn search_exa(query: &str, api_key: &str) -> Result<SearchResults, String> {
    let response = http_client()
        .post("https://api.exa.ai/search")
        .header("x-api-key", api_key)
        .json(&json!({ "query": query, "numResults": 10, "contents": { "text": { "maxCharacters": 300 } } }))
        .send()
        .await
        .map_err(|e| format!("exa search failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("exa search: HTTP {}", response.status()));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("exa parse failed: {e}"))?;
    Ok(body["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .map(|r| {
                    (
                        r["title"].as_str().unwrap_or_default().to_string(),
                        r["url"].as_str().unwrap_or_default().to_string(),
                        r["text"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn search_ddg(query: &str, cancel: &CancellationToken) -> Result<SearchResults, String> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(query));
    let html = tokio::select! {
        _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
        r = http_client().get(&url).send() => r.map_err(|e| format!("search failed: {e}"))?,
    };
    let body = html_to_text(&read_body_capped(html, MAX_BODY_BYTES).await?);
    Ok(parse_ddg_results(&body))
}

async fn search_bing(query: &str, cancel: &CancellationToken) -> Result<SearchResults, String> {
    let url = format!("https://www.bing.com/search?q={}", urlencoding(query));
    let html = tokio::select! {
        _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
        r = http_client().get(&url).send() => r.map_err(|e| format!("bing search failed: {e}"))?,
    };
    // Bing geo-redirects (www.bing.com → cn.bing.com on some egresses);
    // the shared client follows redirects, and the markup shape is the
    // same on both hosts.
    let body = read_body_capped(html, MAX_BODY_BYTES).await?;
    Ok(parse_bing_results(&body))
}

/// Dispatch for the keyless scrape backends (used by the fallback chain).
async fn scrape_search(
    backend: &SearchBackend,
    query: &str,
    cancel: &CancellationToken,
) -> Result<SearchResults, String> {
    match backend {
        SearchBackend::DuckDuckGo => search_ddg(query, cancel).await,
        SearchBackend::Bing => search_bing(query, cancel).await,
        _ => unreachable!("scrape_search is only for keyless backends"),
    }
}

/// The keyless scrape backends, configured one first. On transport error
/// or an empty page (bot wall / layout drift parses to zero results) the
/// caller tries the next one before giving up.
fn scrape_chain(configured: &SearchBackend) -> [SearchBackend; 2] {
    match configured {
        SearchBackend::Bing => [SearchBackend::Bing, SearchBackend::DuckDuckGo],
        _ => [SearchBackend::DuckDuckGo, SearchBackend::Bing],
    }
}

pub struct WebSearchTool {
    services: ToolServices,
}

impl WebSearchTool {
    pub fn new(services: ToolServices) -> Self {
        WebSearchTool { services }
    }
}

impl std::fmt::Debug for WebSearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSearchTool").finish()
    }
}

#[async_trait::async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn label(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web. Returns title, URL, and snippet for the top results."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: &(dyn Fn(AgentToolResult) + Send + Sync),
    ) -> Result<AgentToolResult, String> {
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing required parameter: query".to_string())?;

        let config = &self.services.web_search;
        let results: SearchResults = match &config.backend {
            backend @ (SearchBackend::DuckDuckGo | SearchBackend::Bing) => {
                // Keyless scrapes: a backend that is unreachable from this
                // network (or bot-walled into an empty page) must not kill
                // the tool — fall through to the other one. Only a double
                // failure surfaces an error; one empty page alone still
                // means "no results".
                let mut results = Vec::new();
                let mut last_err: Option<String> = None;
                let mut saw_empty_ok = false;
                for backend in scrape_chain(backend) {
                    match scrape_search(&backend, query, &cancel).await {
                        Ok(r) if !r.is_empty() => {
                            results = r;
                            break;
                        }
                        Ok(_) => saw_empty_ok = true,
                        Err(e) => {
                            tracing::debug!("web_search scrape backend {backend:?} failed: {e}");
                            last_err = Some(e);
                        }
                    }
                }
                if results.is_empty()
                    && !saw_empty_ok
                    && let Some(e) = last_err
                {
                    return Err(e);
                }
                results
            }
            backend => {
                let api_key = config
                    .api_key
                    .clone()
                    .or_else(|| backend.api_key_env().and_then(|env| std::env::var(env).ok()))
                    .ok_or_else(|| {
                        format!(
                            "web search backend {:?} needs an API key (settings webSearch.apiKey or {} env)",
                            backend,
                            backend.api_key_env().unwrap_or("?")
                        )
                    })?;
                tokio::select! {
                    _ = cancel.cancelled() => return Err("Operation aborted".to_string()),
                    r = async {
                        match backend {
                            SearchBackend::Brave => search_brave(query, &api_key).await,
                            SearchBackend::Tavily => search_tavily(query, &api_key).await,
                            SearchBackend::Exa => search_exa(query, &api_key).await,
                            SearchBackend::DuckDuckGo | SearchBackend::Bing => unreachable!(),
                        }
                    } => r?,
                }
            }
        };

        if results.is_empty() {
            return Ok(AgentToolResult::text(format!("no results for {query:?}")));
        }
        let mut out = format!("Search results for {query:?}:\n\n");
        for (i, (title, link, snippet)) in results.iter().take(MAX_SEARCH_RESULTS).enumerate() {
            out.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                title,
                link,
                snippet
            ));
        }
        Ok(AgentToolResult::text(crate::services::wrap_untrusted(
            &self.services,
            &format!("web_search {query:?}"),
            truncate_text(&out, MAX_RESPONSE_CHARS),
        )))
    }
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Parse Bing result HTML: organic results are `<li class="b_algo">`
/// blocks containing `<h2><a href="URL">Title</a></h2>` and a `<p>`
/// snippet. Tag/entity handling reuses html_to_text on the fragments.
/// Falls back to empty on layout drift (the scrape chain then tries the
/// other free backend).
fn parse_bing_results(html: &str) -> Vec<(String, String, String)> {
    fn fragment_text(fragment: &str) -> String {
        html_to_text(fragment)
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string()
    }

    let mut results = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<li class=\"b_algo\"") {
        let after = &rest[start..];
        // b_algo blocks never nest <li>, so the first close tag ends it.
        let block = match after.find("</li>") {
            Some(end) => &after[..end],
            None => after,
        };
        rest = &after[block.len()..];

        let Some(h2) = block.find("<h2").map(|i| &block[i..]) else {
            continue;
        };
        let Some(url) = h2
            .find("href=\"")
            .map(|i| &h2[i + "href=\"".len()..])
            .and_then(|s| s.find('"').map(|end| s[..end].to_string()))
        else {
            continue;
        };
        if !url.starts_with("http") {
            continue;
        }
        let Some(a_open_end) = h2
            .find("<a")
            .and_then(|i| h2[i..].find('>').map(|e| i + e + 1))
        else {
            continue;
        };
        let title = match h2[a_open_end..].find("</a>") {
            Some(end) => fragment_text(&h2[a_open_end..a_open_end + end]),
            None => continue,
        };
        if title.is_empty() {
            continue;
        }
        // `<p>` or `<p ...>` only — `<pre`/`<picture` must not match.
        let snippet = block
            .match_indices("<p")
            .map(|(i, _)| i)
            .find(|&i| {
                block[i + 2..]
                    .chars()
                    .next()
                    .is_some_and(|c| c == '>' || c.is_whitespace())
            })
            .and_then(|i| block[i..].find('>').map(|e| i + e + 1))
            .and_then(|i| block[i..].find("</p>").map(|e| &block[i..i + e]))
            .map(fragment_text)
            .unwrap_or_default();
        results.push((title, url, snippet));
    }
    results
}

/// Parse DuckDuckGo HTML-lite output (after html_to_text): result blocks look
/// like "Title\nURL\nsnippet". Falls back to empty on layout drift.
fn parse_ddg_results(text: &str) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i + 2 < lines.len() {
        let (title, link, snippet) = (lines[i], lines[i + 1], lines[i + 2]);
        if link.starts_with("http") && !title.is_empty() {
            results.push((title.to_string(), link.to_string(), snippet.to_string()));
            i += 3;
        } else {
            i += 1;
        }
    }
    results
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn html_to_text_strips_tags_and_scripts() {
        let html = "<html><head><style>body{color:red}</style><title>T</title></head>\
            <body><h1>Hello</h1><p>World &amp; friends</p><script>var x=1;</script><div>done</div></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("World & friends"), "{text}");
        assert!(text.contains("done"), "{text}");
        assert!(!text.contains("color:red"), "{text}");
        assert!(!text.contains("var x"), "{text}");
    }

    #[test]
    fn ddg_parser_extracts_triples() {
        let text = "Some header\nResult One\nhttps://example.com/1\nsnippet one\nResult Two\nhttps://example.com/2\nsnippet two\nfooter";
        let results = parse_ddg_results(text);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "Result One");
        assert_eq!(results[1].1, "https://example.com/2");
    }

    #[test]
    fn bing_parser_extracts_b_algo_blocks() {
        // Shape mirrors the live markup (www/cn.bing.com): minified, one
        // <li class="b_algo"> per organic result.
        let html = r#"<html><body><ol id="b_results">
            <li class="b_algo"><h2><a href="https://example.com/one" target="_blank">Result <strong>One</strong></a></h2><div class="b_caption"><p>first &amp; best</p></div></li>
            <li class="b_ad"><h2><a href="https://ad.example/x">Ad</a></h2></li>
            <li class="b_algo"><h2><a href="https://example.com/two">Second</a></h2></li>
            </ol></body></html>"#;
        let results = parse_bing_results(html);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].0, "Result One");
        assert_eq!(results[0].1, "https://example.com/one");
        assert_eq!(results[0].2, "first & best");
        assert_eq!(results[1].0, "Second");
        assert_eq!(results[1].2, "");
        // Layout drift (bot wall / consent page) parses to empty, which is
        // what triggers the scrape-chain fallback — never garbage rows.
        assert!(parse_bing_results("<html><body>unusual traffic</body></html>").is_empty());
    }

    #[test]
    fn bing_is_the_default_backend() {
        assert_eq!(SearchBackend::default(), SearchBackend::Bing);
        assert_eq!(SearchBackend::from_setting(None), SearchBackend::Bing);
        assert_eq!(
            SearchBackend::from_setting(Some("bing")),
            SearchBackend::Bing
        );
        assert_eq!(
            SearchBackend::from_setting(Some("duckduckgo")),
            SearchBackend::DuckDuckGo
        );
        assert_eq!(SearchBackend::Bing.api_key_env(), None);
    }

    #[test]
    fn scrape_chain_starts_with_the_configured_backend() {
        assert_eq!(
            scrape_chain(&SearchBackend::Bing),
            [SearchBackend::Bing, SearchBackend::DuckDuckGo]
        );
        assert_eq!(
            scrape_chain(&SearchBackend::DuckDuckGo),
            [SearchBackend::DuckDuckGo, SearchBackend::Bing]
        );
    }

    /// Regression: body reads must be capped — an unbounded text() read let
    /// a hostile/buggy server exhaust memory.
    #[tokio::test]
    async fn body_read_is_capped() {
        let big = vec![b'x'; 1024];
        let response: reqwest::Response = http::Response::new(reqwest::Body::from(big)).into();
        let text = read_body_capped(response, 100).await.unwrap();
        assert_eq!(text.len(), 100);

        // Under the cap: full body returned.
        let response: reqwest::Response =
            http::Response::new(reqwest::Body::from(b"small".to_vec())).into();
        assert_eq!(read_body_capped(response, 100).await.unwrap(), "small");
    }

    #[test]
    fn ssrf_blocked_ip_ranges() {
        use std::net::IpAddr;
        let blocked = [
            "127.0.0.1",        // loopback
            "127.1.2.3",        // loopback range
            "10.0.0.5",         // RFC1918
            "172.16.0.1",       // RFC1918
            "192.168.1.1",      // RFC1918
            "169.254.169.254",  // link-local (cloud metadata)
            "0.0.0.0",          // unspecified
            "100.64.0.1",       // CGNAT
            "::1",              // IPv6 loopback
            "::",               // IPv6 unspecified
            "fe80::1",          // IPv6 link-local
            "fd00::1",          // IPv6 unique-local
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "::ffff:10.1.2.3",  // IPv4-mapped private
            "::ffff:a9fe:a9fe", // IPv4-mapped 169.254.169.254
        ];
        for ip in blocked {
            assert!(
                ip_is_blocked(&ip.parse::<IpAddr>().unwrap()),
                "{ip} must be blocked"
            );
        }
        let allowed = [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ];
        for ip in allowed {
            assert!(
                !ip_is_blocked(&ip.parse::<IpAddr>().unwrap()),
                "{ip} must be allowed"
            );
        }
    }

    /// Regression: redirects must be followed manually with per-hop SSRF
    /// re-validation (reqwest's sync redirect policy cannot run the async
    /// DNS check). These cover the per-hop helpers: target resolution
    /// (relative/absolute) and scheme restriction.
    #[test]
    fn redirect_target_resolution() {
        // Relative redirect resolves against the current URL.
        assert_eq!(
            resolve_redirect("https://example.com/a/b", "../c").unwrap(),
            "https://example.com/c"
        );
        // Absolute redirect is taken as-is.
        assert_eq!(
            resolve_redirect("https://example.com/", "https://other.com/x").unwrap(),
            "https://other.com/x"
        );
        // Protocol-relative keeps the scheme.
        assert_eq!(
            resolve_redirect("https://example.com/", "//other.com/x").unwrap(),
            "https://other.com/x"
        );
        // Non-http(s) targets are refused outright.
        assert!(resolve_redirect("https://example.com/", "file:///etc/passwd").is_err());
        assert!(resolve_redirect("https://example.com/", "data:text/html,hi").is_err());
    }

    /// A redirect to a literal blocked IP is caught by the same per-hop
    /// validation the manual follow loop applies (async DNS not needed for
    /// literal IPs, so this runs without network).
    #[tokio::test]
    async fn redirect_hop_to_private_ip_is_rejected() {
        let next = resolve_redirect(
            "https://example.com/",
            "http://169.254.169.254/latest/meta-data",
        )
        .unwrap();
        let err = ensure_public_url(&next).await.unwrap_err();
        assert!(err.contains("private/local"), "{err}");
    }

    /// Anti-rebinding pinning: hostname URLs yield the validated DNS
    /// answers for pinning on the request; literal-IP URLs carry no
    /// host and need no pinning.
    #[tokio::test]
    async fn validated_target_carries_pinnable_addrs() {
        let target = validate_public_url("http://8.8.8.8/").await.unwrap();
        assert!(target.host.is_none());
        assert!(target.addrs.is_empty());

        // localhost resolves via the hosts file (no external DNS needed)
        // and must be blocked before any pinning happens.
        match validate_public_url("http://localhost:9999/").await {
            Err(err) => assert!(err.contains("private/local"), "{err}"),
            Ok(target) => panic!(
                "localhost must be rejected, got validated addrs {:?}",
                target.addrs
            ),
        }
    }

    /// Literal-IP URLs are rejected/accepted without any DNS lookup.
    #[tokio::test]
    async fn ssrf_literal_ip_urls() {
        for url in [
            "http://127.0.0.1/admin",
            "http://169.254.169.254/latest/meta-data",
            "http://10.0.0.1/",
            "http://192.168.0.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            let err = ensure_public_url(url).await.unwrap_err();
            assert!(err.contains("private/local"), "{url}: {err}");
        }
        // Public literal IPs pass without touching DNS.
        assert!(ensure_public_url("http://8.8.8.8/").await.is_ok());
        assert!(ensure_public_url("https://1.1.1.1/").await.is_ok());
    }
}
