//! Vertex ADC (Application Default Credentials) token source. Port of the
//! google-auth-library chain pi gets via `@google/genai`: service-account
//! JSON (JWT-bearer grant, RS256 self-signed assertion), the gcloud
//! well-known ADC file (authorized_user refresh-token grant), and — as the
//! last resort — the GCE metadata server (probed with a short timeout so
//! non-GCP hosts fail fast).

use std::collections::HashMap;

use serde_json::Value;

/// OAuth token endpoints (SA JWT-bearer + refresh grants) get a bounded
/// per-request timeout: reqwest's default is infinite, so a wedged endpoint
/// (or test mock) would park the provider forever instead of erroring.
const TOKEN_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// gcloud's public OAuth client (from google-auth-library).
const GCLOUD_CLIENT_ID: &str = "32555940559.apps.googleusercontent.com";
const GCLOUD_CLIENT_SECRET: &str = "ZmssLNjJy2998hD4CTg2ejr2";
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// GCE instance metadata server root. Overridable via
/// `TACK_GCE_METADATA_URL` (mainly for tests).
const DEFAULT_METADATA_URL: &str = "http://metadata.google.internal";

#[derive(Clone)]
pub enum GoogleTokenSource {
    ServiceAccount {
        client_email: String,
        private_key: String,
        token_uri: String,
    },
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        token_uri: String,
    },
    /// GCE/Cloud Run/GKE instance metadata server (last-resort ADC).
    GceMetadata { base_url: String },
}

impl std::fmt::Debug for GoogleTokenSource {
    /// private_key / client_secret / refresh_token are secrets: Debug output
    /// (logs, `?` tracing fields) must never show them in plaintext.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GoogleTokenSource::ServiceAccount {
                client_email,
                token_uri,
                ..
            } => f
                .debug_struct("ServiceAccount")
                .field("client_email", client_email)
                .field("private_key", &"[REDACTED]")
                .field("token_uri", token_uri)
                .finish(),
            GoogleTokenSource::AuthorizedUser {
                client_id,
                token_uri,
                ..
            } => f
                .debug_struct("AuthorizedUser")
                .field("client_id", client_id)
                .field("client_secret", &"[REDACTED]")
                .field("refresh_token", &"[REDACTED]")
                .field("token_uri", token_uri)
                .finish(),
            GoogleTokenSource::GceMetadata { base_url } => f
                .debug_struct("GceMetadata")
                .field("base_url", base_url)
                .finish(),
        }
    }
}

/// A resolved ADC source plus the GCP project (when known).
#[derive(Clone, Debug)]
pub struct Adc {
    pub source: GoogleTokenSource,
    pub project: Option<String>,
}

impl GoogleTokenSource {
    /// Stable cache key for the token cache.
    fn fingerprint(&self) -> String {
        use sha2::Digest;
        match self {
            GoogleTokenSource::ServiceAccount { client_email, .. } => format!("sa:{client_email}"),
            GoogleTokenSource::AuthorizedUser { refresh_token, .. } => {
                let hash = sha2::Sha256::digest(refresh_token.as_bytes());
                format!("user:{hash:x}")
            }
            GoogleTokenSource::GceMetadata { base_url } => format!("gce:{base_url}"),
        }
    }

    /// Mint (or return a cached) access token + its expiry.
    pub async fn access_token(
        &self,
        client: &reqwest::Client,
    ) -> Result<(String, std::time::Instant), String> {
        let key = self.fingerprint();
        {
            let cache = token_cache().lock().expect("token cache poisoned");
            if let Some((token, expiry)) = cache.get(&key)
                && std::time::Instant::now() + std::time::Duration::from_secs(60) < *expiry
            {
                return Ok((token.clone(), *expiry));
            }
        }
        let (token, expiry) = self.mint(client).await?;
        token_cache()
            .lock()
            .expect("token cache poisoned")
            .insert(key, (token.clone(), expiry));
        Ok((token, expiry))
    }

    async fn mint(&self, client: &reqwest::Client) -> Result<(String, std::time::Instant), String> {
        match self {
            GoogleTokenSource::ServiceAccount {
                client_email,
                private_key,
                token_uri,
            } => {
                let now = jsonwebtoken::get_current_timestamp();
                #[derive(serde::Serialize)]
                struct Claims<'a> {
                    iss: &'a str,
                    scope: &'a str,
                    aud: &'a str,
                    iat: u64,
                    exp: u64,
                }
                let claims = Claims {
                    iss: client_email,
                    scope: CLOUD_PLATFORM_SCOPE,
                    aud: token_uri,
                    iat: now,
                    exp: now + 3600,
                };
                let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
                let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_bytes())
                    .map_err(|e| format!("invalid service-account private key: {e}"))?;
                let assertion = jsonwebtoken::encode(&header, &claims, &key)
                    .map_err(|e| format!("failed to sign service-account JWT: {e}"))?;
                let body = client
                    .post(token_uri)
                    .form(&[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                        ("assertion", assertion.as_str()),
                    ])
                    .timeout(TOKEN_REQUEST_TIMEOUT)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                parse_token_response(body).await
            }
            GoogleTokenSource::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
                token_uri,
            } => {
                let body = client
                    .post(token_uri)
                    .form(&[
                        ("grant_type", "refresh_token"),
                        ("client_id", client_id.as_str()),
                        ("client_secret", client_secret.as_str()),
                        ("refresh_token", refresh_token.as_str()),
                    ])
                    .timeout(TOKEN_REQUEST_TIMEOUT)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                parse_token_response(body).await
            }
            GoogleTokenSource::GceMetadata { base_url } => {
                let body = client
                    .get(format!(
                        "{}/computeMetadata/v1/instance/service-accounts/default/token",
                        base_url.trim_end_matches('/')
                    ))
                    .header("Metadata-Flavor", "Google")
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                parse_token_response(body).await
            }
        }
    }
}

/// Metadata server root: `TACK_GCE_METADATA_URL` override, else the real
/// GCE metadata server.
fn metadata_base_url() -> String {
    std::env::var("TACK_GCE_METADATA_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_METADATA_URL.to_string())
}

/// GET a metadata server path (plain-text body). None on any failure.
async fn metadata_get(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = client
        .get(url)
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response
        .text()
        .await
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Probe the GCE metadata server (use a short-timeout client — it only
/// answers on GCP). Returns the metadata token source plus the instance
/// project. (The instance zone is NOT consulted for a location — TS
/// `resolveLocation` only honors GOOGLE_CLOUD_LOCATION.)
pub async fn gce_metadata_adc_at(client: &reqwest::Client, base_url: &str) -> Option<Adc> {
    let base = base_url.trim_end_matches('/');
    let project_url = format!("{base}/computeMetadata/v1/project/project-id");
    let project = metadata_get(client, &project_url).await?;
    Some(Adc {
        source: GoogleTokenSource::GceMetadata {
            base_url: base.to_string(),
        },
        project: Some(project),
    })
}

/// Full ADC chain (google-auth-library order): env credentials file → gcloud
/// well-known ADC file → GCE metadata server. Uses a short-timeout client
/// for the metadata probe so non-GCP hosts fail fast.
pub async fn adc_with_metadata_fallback() -> Result<Option<Adc>, String> {
    // Credential-file reads are blocking std::fs — keep them off the async
    // executor's worker threads.
    if let Some(adc) = tokio::task::spawn_blocking(from_adc_env)
        .await
        .map_err(|e| format!("ADC env scan failed: {e}"))??
    {
        return Ok(Some(adc));
    }
    crate::tls::ensure_ring_provider();
    let probe = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(800))
        .build()
        .map_err(|e| e.to_string())?;
    Ok(gce_metadata_adc_at(&probe, &metadata_base_url()).await)
}

async fn parse_token_response(
    response: reqwest::Response,
) -> Result<(String, std::time::Instant), String> {
    let status = response.status();
    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let message = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("token request failed");
        return Err(format!("{status}: {message}"));
    }
    let token = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "token response has no access_token".to_string())?
        .to_string();
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    Ok((
        token,
        std::time::Instant::now() + std::time::Duration::from_secs(expires_in),
    ))
}

fn token_cache() -> &'static std::sync::Mutex<HashMap<String, (String, std::time::Instant)>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<String, (String, std::time::Instant)>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Parse a credentials JSON value (service_account or authorized_user).
pub fn parse_credentials_json(value: &Value) -> Result<Adc, String> {
    match value.get("type").and_then(Value::as_str) {
        Some("service_account") => {
            let get = |key: &str| {
                value
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| format!("service-account JSON missing {key}"))
            };
            Ok(Adc {
                source: GoogleTokenSource::ServiceAccount {
                    client_email: get("client_email")?,
                    private_key: get("private_key")?,
                    token_uri: value
                        .get("token_uri")
                        .and_then(Value::as_str)
                        .unwrap_or(DEFAULT_TOKEN_URI)
                        .to_string(),
                },
                project: value
                    .get("project_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        Some("authorized_user") => {
            let get = |key: &str| {
                value
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| format!("authorized_user JSON missing {key}"))
            };
            Ok(Adc {
                source: GoogleTokenSource::AuthorizedUser {
                    client_id: get("client_id")?,
                    client_secret: get("client_secret")?,
                    refresh_token: get("refresh_token")?,
                    token_uri: value
                        .get("token_uri")
                        .and_then(Value::as_str)
                        .unwrap_or(DEFAULT_TOKEN_URI)
                        .to_string(),
                },
                project: value
                    .get("project_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        other => Err(format!("unsupported credentials type {other:?}")),
    }
}

/// The gcloud well-known ADC file path.
pub fn gcloud_adc_path() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(std::path::PathBuf::from)
            .map(|p| {
                p.join("gcloud")
                    .join("application_default_credentials.json")
            })
    }
    #[cfg(not(windows))]
    {
        dirs::config_dir().map(|p| {
            p.join("gcloud")
                .join("application_default_credentials.json")
        })
    }
}

/// The ADC chain: `GOOGLE_APPLICATION_CREDENTIALS` file, then the gcloud
/// well-known ADC file. Returns None when neither exists.
pub fn from_adc_env() -> Result<Option<Adc>, String> {
    if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        && !path.is_empty()
    {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("GOOGLE_APPLICATION_CREDENTIALS unreadable: {e}"))?;
        let value: Value = serde_json::from_str(&content)
            .map_err(|e| format!("GOOGLE_APPLICATION_CREDENTIALS is not valid JSON: {e}"))?;
        return parse_credentials_json(&value).map(Some);
    }
    if let Some(path) = gcloud_adc_path()
        && path.exists()
    {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("gcloud ADC file unreadable: {e}"))?;
        let value: Value = serde_json::from_str(&content)
            .map_err(|e| format!("gcloud ADC file is not valid JSON: {e}"))?;
        // gcloud writes authorized_user without the gcloud client constants
        // in some flows; fill defaults when absent.
        let mut value = value;
        if value.get("type").and_then(Value::as_str) == Some("authorized_user") {
            let obj = value.as_object_mut().expect("object");
            obj.entry("client_id")
                .or_insert_with(|| Value::String(GCLOUD_CLIENT_ID.into()));
            obj.entry("client_secret")
                .or_insert_with(|| Value::String(GCLOUD_CLIENT_SECRET.into()));
        }
        return parse_credentials_json(&value).map(Some);
    }
    Ok(None)
}

/// Project resolution: env vars first, then the credentials' project_id.
pub fn resolve_project(adc: Option<&Adc>) -> Option<String> {
    std::env::var("GOOGLE_CLOUD_PROJECT")
        .ok()
        .filter(|p| !p.is_empty())
        .or_else(|| {
            std::env::var("GCLOUD_PROJECT")
                .ok()
                .filter(|p| !p.is_empty())
        })
        .or_else(|| adc.and_then(|a| a.project.clone()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Mock GCE metadata server. Serves project/zone/token endpoints,
    /// requires the `Metadata-Flavor: Google` header, and counts token hits.
    struct MockMetadata {
        base_url: String,
        token_hits: Arc<AtomicUsize>,
        server: tokio::task::JoinHandle<()>,
    }

    impl MockMetadata {
        async fn start(project: &str, zone: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let token_hits = Arc::new(AtomicUsize::new(0));
            let hits = token_hits.clone();
            let project = project.to_string();
            let zone = zone.to_string();
            let server = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let hits = hits.clone();
                    let project = project.clone();
                    let zone = zone.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 4096];
                        let n = socket.read(&mut buf).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buf[..n]).to_string();
                        let path = request.split_whitespace().nth(1).unwrap_or("");
                        // reqwest/hyper lowercases header names on the wire.
                        let flavored = request
                            .to_ascii_lowercase()
                            .contains("metadata-flavor: google");
                        let (status, body) = if !flavored {
                            ("403 Forbidden", String::new())
                        } else if path.ends_with("/project/project-id") {
                            ("200 OK", project)
                        } else if path.ends_with("/instance/zone") {
                            ("200 OK", zone)
                        } else if path.ends_with("/service-accounts/default/token") {
                            hits.fetch_add(1, Ordering::SeqCst);
                            (
                                "200 OK",
                                r#"{"access_token":"ya29.mock-token","expires_in":3600,"token_type":"Bearer"}"#
                                    .to_string(),
                            )
                        } else {
                            ("404 Not Found", String::new())
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            });
            MockMetadata {
                base_url: format!("http://{addr}"),
                token_hits,
                server,
            }
        }
    }

    impl Drop for MockMetadata {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    fn short_timeout_client() -> reqwest::Client {
        crate::tls::ensure_ring_provider();
        reqwest::Client::builder()
            .timeout(Duration::from_millis(800))
            .build()
            .unwrap()
    }

    #[test]
    fn token_source_debug_redacts_secrets() {
        let sa = GoogleTokenSource::ServiceAccount {
            client_email: "svc@example.iam".to_string(),
            private_key: "-----BEGIN PRIVATE KEY-----secret-key-material".to_string(),
            token_uri: DEFAULT_TOKEN_URI.to_string(),
        };
        let debug = format!("{sa:?}");
        assert!(!debug.contains("secret-key-material"), "{debug}");
        assert!(debug.contains("svc@example.iam"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");

        let user = GoogleTokenSource::AuthorizedUser {
            client_id: "client-id-1".to_string(),
            client_secret: "super-secret-value".to_string(),
            refresh_token: "refresh-secret-value".to_string(),
            token_uri: DEFAULT_TOKEN_URI.to_string(),
        };
        let debug = format!("{user:?}");
        assert!(!debug.contains("super-secret-value"), "{debug}");
        assert!(!debug.contains("refresh-secret-value"), "{debug}");
        assert!(debug.contains("client-id-1"), "{debug}");
    }

    #[tokio::test]
    async fn metadata_adc_resolves_project() {
        let mock =
            MockMetadata::start("test-project-123", "projects/123/zones/us-central1-a").await;
        let adc = gce_metadata_adc_at(&short_timeout_client(), &mock.base_url)
            .await
            .expect("metadata ADC should resolve");
        assert_eq!(adc.project.as_deref(), Some("test-project-123"));
        let GoogleTokenSource::GceMetadata { base_url } = &adc.source else {
            panic!("expected GceMetadata source");
        };
        assert_eq!(base_url, &mock.base_url);
    }

    #[tokio::test]
    async fn metadata_token_fetched_and_cached_by_expiry() {
        let mock = MockMetadata::start("test-project-123", "projects/123/zones/us-east4-b").await;
        let adc = gce_metadata_adc_at(&short_timeout_client(), &mock.base_url)
            .await
            .expect("metadata ADC should resolve");
        let client = reqwest::Client::new();
        let (token, expiry) = adc.source.access_token(&client).await.unwrap();
        assert_eq!(token, "ya29.mock-token");
        assert!(expiry > Instant::now() + Duration::from_secs(3500));
        // Second call must hit the cache, not the server.
        let (cached, _) = adc.source.access_token(&client).await.unwrap();
        assert_eq!(cached, token);
        assert_eq!(mock.token_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn metadata_probe_fails_fast_when_server_unreachable() {
        // Loopback port 9 (discard) refuses connections immediately.
        let start = Instant::now();
        let adc = gce_metadata_adc_at(&short_timeout_client(), "http://127.0.0.1:9").await;
        assert!(adc.is_none());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn metadata_probe_times_out_when_server_hangs() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                // Accept and never answer.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    drop(socket);
                });
            }
        });
        let start = Instant::now();
        let adc = gce_metadata_adc_at(&short_timeout_client(), &format!("http://{addr}")).await;
        assert!(adc.is_none());
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "probe must fail fast, took {elapsed:?}"
        );
        server.abort();
    }
}
