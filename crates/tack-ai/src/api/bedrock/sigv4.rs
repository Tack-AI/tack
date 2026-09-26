//! AWS Signature Version 4 signing (port of what `@smithy/signature-v4` does
//! for the TS adapter). Generic over service/region so the AWS published
//! known-answer tests can drive it.

use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use sha2::Digest;

/// Static credentials (plus optional session token).
#[derive(Clone, PartialEq)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    /// secret_key / session_token are secrets: Debug output (logs, `?`
    /// tracing fields) must never show them in plaintext. The access key id
    /// is an identifier, not a secret, so it stays visible.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key", &self.access_key)
            .field("secret_key", &"[REDACTED]")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Everything needed to sign one request.
#[derive(Clone, Debug, PartialEq)]
pub struct SignParams {
    pub method: String,
    pub host: String,
    /// Already-normalized path (segments AWS-encoded; see `encode_path`).
    pub path: String,
    /// Raw query pairs; canonicalized (encoded + sorted) during signing.
    pub query: Vec<(String, String)>,
    pub region: String,
    /// e.g. "bedrock".
    pub service: String,
    /// Headers to sign (host/x-amz-date/x-amz-content-sha256 are added).
    pub headers: BTreeMap<String, String>,
    pub payload: Vec<u8>,
    /// Seconds since epoch (injected for deterministic tests).
    pub timestamp: u64,
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// AWS URI encoding: unreserved characters stay, everything else is
/// percent-encoded uppercase (RFC 3986 set per the SigV4 spec).
pub fn aws_uri_encode(value: &str, encode_slash: bool) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b'/' if !encode_slash => out.push('/'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Encode a path segment-wise, preserving `/` separators. Use
/// `aws_uri_encode(id, true)` for a single path *label* (model ids with
/// slashes, e.g. inference-profile ARNs).
pub fn encode_path(path: &str) -> String {
    aws_uri_encode(path, false)
}

fn format_amz_date(timestamp: u64) -> (String, String) {
    // Civil-from-days (Howard Hinnant) to avoid a chrono dependency here.
    let days = (timestamp / 86_400) as i64;
    let secs = timestamp % 86_400;
    let (hour, minute, second) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
        format!("{year:04}{month:02}{day:02}"),
    )
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Sign a request, returning the headers to send: the input headers plus
/// `host`, `x-amz-date`, `x-amz-security-token` (when present), and the final
/// `authorization` header. `x-amz-content-sha256` is signed only when the
/// caller includes it in `params.headers` (Bedrock does; the AWS docs
/// example does not).
pub fn sign(params: &SignParams, credentials: &Credentials) -> BTreeMap<String, String> {
    let (amz_date, date) = format_amz_date(params.timestamp);
    let payload_hash = hex_encode(&sha2::Sha256::digest(&params.payload));

    let mut headers: BTreeMap<String, String> = params
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.insert("host".to_string(), params.host.clone());
    headers.insert("x-amz-date".to_string(), amz_date.clone());
    if let Some(token) = &credentials.session_token {
        headers.insert("x-amz-security-token".to_string(), token.clone());
    }

    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers.keys().cloned().collect::<Vec<_>>().join(";");

    let canonical_query = {
        let mut pairs: Vec<(String, String)> = params
            .query
            .iter()
            .map(|(k, v)| (aws_uri_encode(k, true), aws_uri_encode(v, true)))
            .collect();
        pairs.sort();
        pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    };

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        params.method,
        encode_path(&params.path),
        canonical_query,
        canonical_headers,
        signed_headers,
        payload_hash,
    );

    let scope = format!("{date}/{}/{}/aws4_request", params.region, params.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex_encode(&sha2::Sha256::digest(canonical_request.as_bytes()))
    );

    let k_date = hmac(
        format!("AWS4{}", credentials.secret_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, params.region.as_bytes());
    let k_service = hmac(&k_region, params.service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex_encode(&hmac(&k_signing, string_to_sign.as_bytes()));

    let mut out = params.headers.clone();
    out.insert("host".to_string(), params.host.clone());
    out.insert("x-amz-date".to_string(), amz_date);
    if let Some(token) = &credentials.session_token {
        out.insert("x-amz-security-token".to_string(), token.clone());
    }
    out.insert(
        "authorization".to_string(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key
        ),
    );
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// AWS documentation "complete signing process" example: GET
    /// iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08 with the
    /// documented test credentials and timestamp.
    #[test]
    fn aws_docs_known_answer() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "content-type".to_string(),
            "application/x-www-form-urlencoded; charset=utf-8".to_string(),
        );
        let params = SignParams {
            method: "GET".to_string(),
            host: "iam.amazonaws.com".to_string(),
            path: "/".to_string(),
            query: vec![
                ("Action".to_string(), "ListUsers".to_string()),
                ("Version".to_string(), "2010-05-08".to_string()),
            ],
            region: "us-east-1".to_string(),
            service: "iam".to_string(),
            headers,
            payload: Vec::new(),
            // 2015-08-30T12:36:00Z
            timestamp: 1_440_938_160,
        };
        let credentials = Credentials {
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        };
        let signed = sign(&params, &credentials);
        assert_eq!(
            signed["authorization"],
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn uri_encoding_rules() {
        assert_eq!(
            aws_uri_encode("us.anthropic.claude-sonnet-4-5", true),
            "us.anthropic.claude-sonnet-4-5"
        );
        assert_eq!(
            aws_uri_encode("arn:aws:bedrock:us-east-1:123:inference-profile/us.x", true),
            "arn%3Aaws%3Abedrock%3Aus-east-1%3A123%3Ainference-profile%2Fus.x"
        );
        assert_eq!(
            encode_path("/model/a%2Fb/converse-stream"),
            "/model/a%252Fb/converse-stream"
        );
    }

    #[test]
    fn amz_date_format() {
        let (amz, date) = format_amz_date(1_440_938_160);
        assert_eq!(amz, "20150830T123600Z");
        assert_eq!(date, "20150830");
    }
}
