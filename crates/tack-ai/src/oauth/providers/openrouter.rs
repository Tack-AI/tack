//! OpenRouter OAuth (PKCE → permanent API key). Port of
//! `auth/oauth/openrouter.ts`: the exchanged credential never expires;
//! `refresh` is identity.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::oauth::{BoxFuture, BrowserFlow, OAuthCredential, OAuthError, OAuthFlow, ResolvedAuth};

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const KEYS_URL: &str = "https://openrouter.ai/api/v1/auth/keys";

#[derive(Debug)]
pub struct OpenRouterOAuth;

impl OAuthFlow for OpenRouterOAuth {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    fn start_browser<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Option<BrowserFlow>, OAuthError>> {
        Box::pin(async move {
            // Ephemeral-port callback: the app binds first and passes the
            // port via options.
            let port = options
                .get("callback_port")
                .and_then(|p| p.parse::<u16>().ok())
                .filter(|p| *p != 0)
                .ok_or_else(|| {
                    OAuthError::Other("openrouter flow needs a bound callback_port".into())
                })?;
            let path = format!("/oauth/callback/{}", crate::oauth::random_hex(16));
            let redirect_uri = format!("http://127.0.0.1:{port}{path}");
            let verifier = crate::oauth::generate_verifier();
            let challenge = crate::oauth::challenge_s256(&verifier);
            let mut url =
                reqwest::Url::parse(AUTHORIZE_URL).map_err(|e| OAuthError::Other(e.to_string()))?;
            url.query_pairs_mut()
                .append_pair("callback_url", &redirect_uri)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256");
            Ok(Some(BrowserFlow {
                authorize_url: url.to_string(),
                redirect_uri,
                state: String::new(), // openrouter does not use state
                verifier,
                callback_port: port,
                callback_path: path,
            }))
        })
    }

    fn exchange_code<'a>(
        &'a self,
        client: &'a reqwest::Client,
        flow: &'a BrowserFlow,
        code: &'a str,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        Box::pin(async move {
            let body = crate::oauth::post_json(
                client,
                KEYS_URL,
                &json!({
                    "code": code,
                    "code_verifier": flow.verifier,
                    "code_challenge_method": "S256",
                }),
            )
            .await?;
            let key = body
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| OAuthError::Server("key exchange returned no key".into()))?;
            Ok(OAuthCredential::new(
                key.to_string(),
                String::new(),
                i64::MAX,
            ))
        })
    }

    fn refresh<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        // Permanent API key: nothing to refresh.
        let credential = credential.clone();
        Box::pin(async move { Ok(credential) })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ResolvedAuth {
        ResolvedAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        }
    }
}
