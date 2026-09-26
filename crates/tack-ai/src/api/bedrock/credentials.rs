//! Bedrock credential resolution (port of the AWS SDK default chain adapted
//! to `bedrock-converse-stream.ts`): bearer token > profile (static keys or
//! SSO) > env triple > web identity > ECS container > IMDSv2 instance
//! metadata > default profile > skip-auth dummy. Network fallbacks use short
//! timeouts so non-AWS environments fail fast; fetched credentials are cached
//! until expiry.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use super::sigv4::Credentials;

/// How a Bedrock request authenticates.
#[derive(Clone, PartialEq)]
pub enum BedrockAuth {
    /// `Authorization: Bearer <token>` (Bedrock API key); skips SigV4.
    Bearer(String),
    /// SigV4 with these credentials.
    SigV4(Credentials),
}

impl std::fmt::Debug for BedrockAuth {
    /// The bearer token is a secret: Debug output must never show it in
    /// plaintext (SigV4 credentials redact themselves).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BedrockAuth::Bearer(_) => f.debug_tuple("Bearer").field(&"[REDACTED]").finish(),
            BedrockAuth::SigV4(credentials) => f.debug_tuple("SigV4").field(credentials).finish(),
        }
    }
}

/// Resolution inputs, injectable for tests.
pub struct CredentialEnv<'a> {
    pub get_env: &'a (dyn Fn(&str) -> Option<String> + Send + Sync + 'a),
    pub home: Option<&'a Path>,
    /// `options.api_key` from the request.
    pub api_key: Option<&'a str>,
}

impl std::fmt::Debug for CredentialEnv<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialEnv").finish_non_exhaustive()
    }
}

/// Cache for fetched credentials (SSO/ECS/IMDS), keyed by source.
static CRED_CACHE: Mutex<Option<(String, Credentials, i64)>> = Mutex::new(None);

fn cached(key: &str) -> Option<Credentials> {
    let cache = CRED_CACHE.lock().ok()?;
    let (k, creds, expires) = cache.as_ref()?;
    if k == key && *expires > now_secs() + 60 {
        Some(creds.clone())
    } else {
        None
    }
}

fn store(key: String, creds: Credentials, expires: Option<i64>) {
    let expiry = expires.unwrap_or(now_secs() + 900);
    if let Ok(mut cache) = CRED_CACHE.lock() {
        *cache = Some((key, creds, expiry));
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub async fn resolve(env: &CredentialEnv<'_>) -> Result<BedrockAuth, String> {
    let get = |key: &str| (env.get_env)(key).filter(|v| !v.is_empty());

    // 1. Bearer token (Bedrock API key). AWS_BEDROCK_SKIP_AUTH=1 forces the
    //    SigV4 dummy path instead (proxy setups).
    let skip_auth = get("AWS_BEDROCK_SKIP_AUTH").is_some_and(|v| v == "1");
    if !skip_auth
        && let Some(token) = env
            .api_key
            .map(str::to_string)
            .or_else(|| get("AWS_BEARER_TOKEN_BEDROCK"))
    {
        return Ok(BedrockAuth::Bearer(token));
    }

    // 2. Explicit profile via shared config files (static keys or SSO).
    if let Some(profile) = get("AWS_PROFILE")
        && let Some(home) = env.home
    {
        if let Some(credentials) = read_profile_async(home, &profile).await {
            return Ok(BedrockAuth::SigV4(credentials));
        }
        if let Some(credentials) = sso_profile(home, &profile).await {
            return Ok(BedrockAuth::SigV4(credentials));
        }
    }

    // 3. Environment triple.
    if let (Some(access_key), Some(secret_key)) =
        (get("AWS_ACCESS_KEY_ID"), get("AWS_SECRET_ACCESS_KEY"))
    {
        return Ok(BedrockAuth::SigV4(Credentials {
            access_key,
            secret_key,
            session_token: get("AWS_SESSION_TOKEN"),
        }));
    }

    // 4. Web identity (EKS/Lambda-style): token file + role ARN → STS.
    //    (Env values are captured before any await so no `&dyn Fn` crosses
    //    an await point and the future stays Send.)
    let web_identity_token_file = get("AWS_WEB_IDENTITY_TOKEN_FILE");
    let role_arn = get("AWS_ROLE_ARN");
    if let Some(credentials) = web_identity(
        web_identity_token_file,
        role_arn,
        get("AWS_ROLE_SESSION_NAME"),
        get("AWS_REGION").or_else(|| get("AWS_DEFAULT_REGION")),
    )
    .await
    {
        return Ok(BedrockAuth::SigV4(credentials));
    }

    // 5. ECS container credentials.
    if let Some(credentials) = ecs_credentials(
        get("AWS_CONTAINER_CREDENTIALS_FULL_URI"),
        get("AWS_CONTAINER_AUTHORIZATION_TOKEN"),
        get("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"),
    )
    .await
    {
        return Ok(BedrockAuth::SigV4(credentials));
    }

    // 6. Default profile from shared files (no explicit AWS_PROFILE).
    if env.api_key.is_none()
        && let Some(home) = env.home
    {
        if let Some(credentials) = read_profile_async(home, "default").await {
            return Ok(BedrockAuth::SigV4(credentials));
        }
        if let Some(credentials) = sso_profile(home, "default").await {
            return Ok(BedrockAuth::SigV4(credentials));
        }
    }

    // 7. IMDSv2 instance metadata (last resort; ~1s timeout off-AWS).
    if let Some(credentials) = imds_credentials().await {
        return Ok(BedrockAuth::SigV4(credentials));
    }

    // 8. Skip-auth dummy credentials (SigV4 against a proxy that ignores it).
    if skip_auth {
        return Ok(BedrockAuth::SigV4(Credentials {
            access_key: "dummy-access-key".to_string(),
            secret_key: "dummy-secret-key".to_string(),
            session_token: None,
        }));
    }

    Err(
        "no Bedrock credentials: set AWS_BEARER_TOKEN_BEDROCK, AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, or AWS_PROFILE"
            .to_string(),
    )
}

/// Resolve from the process environment.
pub async fn resolve_from_process(api_key: Option<&str>) -> Result<BedrockAuth, String> {
    let home = dirs::home_dir();
    let env = CredentialEnv {
        get_env: &|key| std::env::var(key).ok(),
        home: home.as_deref(),
        api_key,
    };
    resolve(&env).await
}

/// Read `[profile]` credentials from ~/.aws/credentials (+ region from
/// ~/.aws/config is the caller's concern).
fn read_profile(home: &Path, profile: &str) -> Option<Credentials> {
    let content = std::fs::read_to_string(home.join(".aws").join("credentials")).ok()?;
    let sections = parse_ini(&content);
    let section = sections.get(profile)?;
    Some(Credentials {
        access_key: section.get("aws_access_key_id")?.clone(),
        secret_key: section.get("aws_secret_access_key")?.clone(),
        session_token: section.get("aws_session_token").cloned(),
    })
}

/// Async wrapper: the blocking std::fs read stays off the executor's
/// worker threads.
async fn read_profile_async(home: &Path, profile: &str) -> Option<Credentials> {
    let home = home.to_path_buf();
    let profile = profile.to_string();
    tokio::task::spawn_blocking(move || read_profile(&home, &profile))
        .await
        .ok()
        .flatten()
}

/// Region from ~/.aws/config for a profile (sections are `profile <name>`
/// there, except `default`).
pub fn profile_region(home: &Path, profile: &str) -> Option<String> {
    let content = std::fs::read_to_string(home.join(".aws").join("config")).ok()?;
    let sections = parse_ini(&content);
    let key = if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    };
    sections.get(&key)?.get("region").cloned()
}

fn config_section(home: &Path, profile: &str) -> Option<BTreeMap<String, String>> {
    let content = std::fs::read_to_string(home.join(".aws").join("config")).ok()?;
    let sections = parse_ini(&content);
    let key = if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    };
    sections.get(&key).cloned()
}

/// HTTP client for the metadata/STS calls: fails fast when off-AWS.
fn meta_client(timeout_ms: u64) -> Option<reqwest::Client> {
    crate::tls::ensure_ring_provider();
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
        .ok()
}

/// SSO profile: ~/.aws/config sso_* keys + cached OIDC token → GetRoleCredentials.
async fn sso_profile(home: &Path, profile: &str) -> Option<Credentials> {
    // Config + token-cache reads are blocking std::fs — keep them off the
    // async executor's worker threads.
    let home_buf = home.to_path_buf();
    let profile_owned = profile.to_string();
    let (start_url, sso_region, account_id, role_name, access_token) =
        tokio::task::spawn_blocking(move || {
            let section = config_section(&home_buf, &profile_owned)?;
            let start_url = section.get("sso_start_url")?.clone();
            let sso_region = section.get("sso_region")?.clone();
            let account_id = section.get("sso_account_id")?.clone();
            let role_name = section.get("sso_role_name")?.clone();

            // Cached token from `aws sso login`:
            // ~/.aws/sso/cache/<sha1(start_url)>.json
            use sha1::Digest as _;
            let hash = hex_lower(&sha1::Sha1::digest(start_url.as_bytes()));
            let token_file = home_buf
                .join(".aws")
                .join("sso")
                .join("cache")
                .join(format!("{hash}.json"));
            let token_json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(token_file).ok()?).ok()?;
            let access_token = token_json.get("accessToken")?.as_str()?.to_string();
            // Expired cached tokens are useless.
            if let Some(expires_at) = token_json.get("expiresAt").and_then(|v| v.as_str())
                && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(expires_at)
                && dt.timestamp() <= now_secs()
            {
                return None;
            }
            Some((start_url, sso_region, account_id, role_name, access_token))
        })
        .await
        .ok()??;

    let cache_key = format!("sso:{start_url}:{account_id}:{role_name}");
    if let Some(creds) = cached(&cache_key) {
        return Some(creds);
    }

    let client = meta_client(3_000)?;
    let response = client
        .get(format!(
            "https://portal.sso.{sso_region}.amazonaws.com/federation/credentials?account_id={account_id}&role_name={role_name}"
        ))
        .header("x-amz-sso_bearer_token", &access_token)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    let rc = body.get("roleCredentials")?;
    let creds = Credentials {
        access_key: rc.get("accessKeyId")?.as_str()?.to_string(),
        secret_key: rc.get("secretAccessKey")?.as_str()?.to_string(),
        session_token: rc
            .get("sessionToken")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    };
    let expires = rc
        .get("expiration")
        .and_then(|v| v.as_i64())
        .map(|ms| ms / 1000);
    store(cache_key, creds.clone(), expires);
    Some(creds)
}

/// Web identity: AWS_WEB_IDENTITY_TOKEN_FILE + AWS_ROLE_ARN → STS
/// AssumeRoleWithWebIdentity (unsigned form POST).
async fn web_identity(
    token_file: Option<String>,
    role_arn: Option<String>,
    session_name: Option<String>,
    region: Option<String>,
) -> Option<Credentials> {
    let token_file = token_file?;
    let role_arn = role_arn?;
    let cache_key = format!("webidentity:{token_file}:{role_arn}");
    if let Some(creds) = cached(&cache_key) {
        return Some(creds);
    }
    let token = tokio::task::spawn_blocking(move || std::fs::read_to_string(&token_file))
        .await
        .ok()?
        .ok()?
        .trim()
        .to_string();
    let session_name = session_name.unwrap_or_else(|| "tack".to_string());
    let region = region.unwrap_or_else(|| "us-east-1".to_string());

    let client = meta_client(5_000)?;
    let response = client
        .post(format!("https://sts.{region}.amazonaws.com/"))
        .form(&[
            ("Action", "AssumeRoleWithWebIdentity"),
            ("Version", "2011-06-15"),
            ("RoleArn", role_arn.as_str()),
            ("RoleSessionName", session_name.as_str()),
            ("WebIdentityToken", token.as_str()),
        ])
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.text().await.ok()?;
    let creds = Credentials {
        access_key: xml_tag(&body, "AccessKeyId")?,
        secret_key: xml_tag(&body, "SecretAccessKey")?,
        session_token: xml_tag(&body, "SessionToken"),
    };
    let expires = xml_tag(&body, "Expiration")
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.timestamp());
    store(cache_key, creds.clone(), expires);
    Some(creds)
}

/// ECS container credentials (FULL_URI or RELATIVE_URI).
async fn ecs_credentials(
    full_uri: Option<String>,
    auth_token: Option<String>,
    relative_uri: Option<String>,
) -> Option<Credentials> {
    let (url, auth_token) = match (full_uri, relative_uri) {
        (Some(uri), _) => (uri, auth_token),
        (None, Some(relative)) => (format!("http://169.254.170.2{relative}"), None),
        (None, None) => return None,
    };
    if let Some(creds) = cached(&url) {
        return Some(creds);
    }
    let client = meta_client(2_000)?;
    let mut request = client.get(&url);
    if let Some(token) = auth_token {
        request = request.header("Authorization", token);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    let creds = Credentials {
        access_key: body.get("AccessKeyId")?.as_str()?.to_string(),
        secret_key: body.get("SecretAccessKey")?.as_str()?.to_string(),
        session_token: body
            .get("Token")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    };
    let expires = body
        .get("Expiration")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp());
    store(url, creds.clone(), expires);
    Some(creds)
}

/// IMDSv2: token → role name → role credentials. Short timeouts — on
/// non-AWS hosts this must fail fast.
async fn imds_credentials() -> Option<Credentials> {
    if let Some(creds) = cached("imds") {
        return Some(creds);
    }
    let client = meta_client(1_000)?;
    let token = client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let role = client
        .get("http://169.254.169.254/latest/meta-data/iam/security-credentials/")
        .header("X-aws-ec2-metadata-token", &token)
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let role = role.lines().next()?.trim().to_string();
    if role.is_empty() {
        return None;
    }
    let body: serde_json::Value = client
        .get(format!(
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/{role}"
        ))
        .header("X-aws-ec2-metadata-token", &token)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let creds = Credentials {
        access_key: body.get("AccessKeyId")?.as_str()?.to_string(),
        secret_key: body.get("SecretAccessKey")?.as_str()?.to_string(),
        session_token: body
            .get("Token")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    };
    let expires = body
        .get("Expiration")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp());
    store("imds".to_string(), creds.clone(), expires);
    Some(creds)
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract `<tag>value</tag>` from a small XML body (STS responses).
fn xml_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].to_string())
}

/// Minimal INI parser: `[section]` headers, `key = value` lines, `;`/`#`
/// comments.
fn parse_ini(content: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = Some(name.trim().to_string());
            continue;
        }
        if let (Some(section), Some((key, value))) = (&current, line.split_once('=')) {
            sections
                .entry(section.clone())
                .or_default()
                .insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn env_with<'a>(
        vars: &[(&str, &str)],
        home: Option<&'a Path>,
        api_key: Option<&'a str>,
    ) -> CredentialEnv<'a> {
        let vars: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        // Leak the map + closure for the closure lifetime (tests only).
        let vars: &'static BTreeMap<String, String> = Box::leak(Box::new(vars));
        let get_env: &'static (dyn Fn(&str) -> Option<String> + Send + Sync) =
            Box::leak(Box::new(move |key: &str| vars.get(key).cloned()));
        CredentialEnv {
            get_env,
            home,
            api_key,
        }
    }

    #[test]
    fn debug_redacts_secrets() {
        let bearer = BedrockAuth::Bearer("bedrock-bearer-secret".to_string());
        let debug = format!("{bearer:?}");
        assert!(!debug.contains("bedrock-bearer-secret"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");

        let sigv4 = BedrockAuth::SigV4(Credentials {
            access_key: "AKID".to_string(),
            secret_key: "aws-secret-key-plaintext".to_string(),
            session_token: Some("aws-session-token-plaintext".to_string()),
        });
        let debug = format!("{sigv4:?}");
        assert!(!debug.contains("aws-secret-key-plaintext"), "{debug}");
        assert!(!debug.contains("aws-session-token-plaintext"), "{debug}");
        assert!(debug.contains("AKID"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
    }

    #[tokio::test]
    async fn bearer_wins_over_env_keys() {
        let env = env_with(
            &[
                ("AWS_BEARER_TOKEN_BEDROCK", "bearer-1"),
                ("AWS_ACCESS_KEY_ID", "AK"),
                ("AWS_SECRET_ACCESS_KEY", "SK"),
            ],
            None,
            None,
        );
        assert_eq!(
            resolve(&env).await.unwrap(),
            BedrockAuth::Bearer("bearer-1".to_string())
        );
    }

    #[tokio::test]
    async fn env_triple_with_session_token() {
        let env = env_with(
            &[
                ("AWS_ACCESS_KEY_ID", "AK"),
                ("AWS_SECRET_ACCESS_KEY", "SK"),
                ("AWS_SESSION_TOKEN", "ST"),
            ],
            None,
            None,
        );
        let BedrockAuth::SigV4(creds) = resolve(&env).await.unwrap() else {
            panic!("expected sigv4")
        };
        assert_eq!(creds.access_key, "AK");
        assert_eq!(creds.session_token.as_deref(), Some("ST"));
    }

    #[tokio::test]
    async fn profile_from_shared_file() {
        let dir = tempfile::tempdir().unwrap();
        let aws = dir.path().join(".aws");
        std::fs::create_dir_all(&aws).unwrap();
        std::fs::write(
            aws.join("credentials"),
            "[default]\naws_access_key_id = DEF\naws_secret_access_key = DEFSK\n\n[prod]\naws_access_key_id = PROD\naws_secret_access_key = PRODSK\n",
        )
        .unwrap();
        let env = env_with(&[("AWS_PROFILE", "prod")], Some(dir.path()), None);
        let BedrockAuth::SigV4(creds) = resolve(&env).await.unwrap() else {
            panic!("expected sigv4")
        };
        assert_eq!(creds.access_key, "PROD");

        // Default profile fallback without AWS_PROFILE.
        let env = env_with(&[], Some(dir.path()), None);
        let BedrockAuth::SigV4(creds) = resolve(&env).await.unwrap() else {
            panic!("expected sigv4")
        };
        assert_eq!(creds.access_key, "DEF");
    }

    #[tokio::test]
    async fn skip_auth_dummy_credentials() {
        let env = env_with(
            &[
                ("AWS_BEDROCK_SKIP_AUTH", "1"),
                ("AWS_BEARER_TOKEN_BEDROCK", "ignored"),
            ],
            None,
            None,
        );
        let BedrockAuth::SigV4(creds) = resolve(&env).await.unwrap() else {
            panic!("expected sigv4")
        };
        assert_eq!(creds.access_key, "dummy-access-key");
    }

    #[tokio::test]
    async fn nothing_configured_errors() {
        let env = env_with(&[], None, None);
        assert!(resolve(&env).await.is_err());
    }

    #[tokio::test]
    async fn sso_profile_from_config_and_token_cache() {
        let dir = tempfile::tempdir().unwrap();
        let aws = dir.path().join(".aws");
        std::fs::create_dir_all(&aws).unwrap();
        std::fs::write(
            aws.join("config"),
            "[profile sso-prod]\nsso_start_url = https://d-abc123.awsapps.com/start\nsso_region = us-east-1\nsso_account_id = 123456789012\nsso_role_name = Admin\n",
        )
        .unwrap();
        // SSO token cache: sha1(start_url) as filename.
        use sha1::Digest as _;
        let hash = super::hex_lower(&sha1::Sha1::digest(b"https://d-abc123.awsapps.com/start"));
        let cache_dir = aws.join("sso").join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join(format!("{hash}.json")),
            r#"{"accessToken": "sso-token", "expiresAt": "2999-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        // The SSO HTTP call itself will fail (no network in tests) — the
        // point is the config/token parsing runs and resolution falls through
        // to the error rather than panicking.
        let env = env_with(&[("AWS_PROFILE", "sso-prod")], Some(dir.path()), None);
        assert!(resolve(&env).await.is_err());
    }
}
