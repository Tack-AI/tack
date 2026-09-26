//! Anthropic (Claude Pro/Max subscription) OAuth. Port of
//! `auth/oauth/anthropic.ts`: PKCE S256 with the verifier doubling as
//! `state`, loopback callback on port 53692.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;

use crate::oauth::{BoxFuture, BrowserFlow, OAuthCredential, OAuthError, OAuthFlow, ResolvedAuth};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

#[derive(Debug)]
pub struct AnthropicOAuth;

impl OAuthFlow for AnthropicOAuth {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    fn refresh_skew(&self) -> Duration {
        Duration::from_secs(5 * 60)
    }

    fn start_browser<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        _options: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Option<BrowserFlow>, OAuthError>> {
        Box::pin(async {
            let verifier = crate::oauth::generate_verifier();
            let challenge = crate::oauth::challenge_s256(&verifier);
            let mut url =
                reqwest::Url::parse(AUTHORIZE_URL).map_err(|e| OAuthError::Other(e.to_string()))?;
            url.query_pairs_mut()
                .append_pair("code", "true")
                .append_pair("client_id", CLIENT_ID)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", REDIRECT_URI)
                .append_pair("scope", SCOPES)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                // The PKCE verifier doubles as state (per TS pi).
                .append_pair("state", &verifier);
            Ok(Some(BrowserFlow {
                authorize_url: url.to_string(),
                redirect_uri: REDIRECT_URI.to_string(),
                state: verifier.clone(),
                verifier,
                callback_port: 53692,
                callback_path: "/callback".to_string(),
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
                TOKEN_URL,
                &json!({
                    "grant_type": "authorization_code",
                    "client_id": CLIENT_ID,
                    "code": code,
                    "state": flow.state,
                    "redirect_uri": flow.redirect_uri,
                    "code_verifier": flow.verifier,
                }),
            )
            .await?;
            crate::oauth::token_credential(&body, self.refresh_skew(), None)
        })
    }

    fn refresh<'a>(
        &'a self,
        client: &'a reqwest::Client,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<OAuthCredential, OAuthError>> {
        Box::pin(async move {
            let body = crate::oauth::post_json(
                client,
                TOKEN_URL,
                &json!({
                    "grant_type": "refresh_token",
                    "client_id": CLIENT_ID,
                    "refresh_token": credential.refresh,
                }),
            )
            .await?;
            let mut fresh = crate::oauth::token_credential(
                &body,
                self.refresh_skew(),
                Some(&credential.refresh),
            )?;
            fresh.extras.clone_from(&credential.extras);
            Ok(fresh)
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ResolvedAuth {
        // The anthropic-messages adapter detects `sk-ant-oat` tokens and
        // switches to Bearer + Claude Code identity headers automatically.
        ResolvedAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        }
    }
}
