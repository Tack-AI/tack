//! Transport utilities shared by the v3 carriers (process + WASM):
//! bounded NDJSON line reading and plugin-child credential stripping.
//!
//! (The v1 `PluginPeer`/`PluginProcess`/`HostServices` that used to live
//! here were removed with the v3 protocol switch; see
//! `docs/plugin-roadmap.md` — the v3 peer is `v3::JsonRpcPeer`, the v3
//! process carrier is `v3::V3Process`.)

use tokio::io::AsyncBufReadExt;

/// Hard cap on one NDJSON line. A broken or hostile plugin that streams
/// without ever emitting '\n' would otherwise grow the read buffer without
/// bound (lines() has no limit); past the cap the peer is declared dead.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// What [`read_line_bounded`] does once a line exceeds [`MAX_LINE_BYTES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverCap {
    /// Return Err immediately (the protocol read pump: the peer is
    /// declared dead, so there is no point draining the rest of the
    /// hostile line).
    Fail,
    /// Consume and discard bytes until the terminating '\n', THEN return
    /// Err (stderr forwarders: the reader must keep making progress so
    /// the writer never blocks on a full pipe, and must not spin on the
    /// same unconsumed chunk).
    Discard,
}

/// Read one '\n'-terminated line with a hard size cap. Returns Ok(None) on
/// clean EOF (no bytes), Ok(Some(line)) for a line (terminator and a
/// trailing '\r' stripped), Err on IO error or an over-cap line.
///
/// Shared by the v3 peer's read pump and both carriers' stderr
/// forwarders: a plugin writing stderr without newlines must not grow the
/// host buffer without bound either.
pub async fn read_line_bounded<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    on_over_cap: OverCap,
) -> std::io::Result<Option<String>>
where
    R: AsyncBufReadExt + Unpin,
{
    fn over_cap_err() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("plugin line exceeds {} byte cap", MAX_LINE_BYTES),
        )
    }
    buf.clear();
    let mut over_cap = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            // EOF: a partial line without terminator is still delivered
            // (matches Lines::next_line); an over-cap partial is an error.
            if over_cap {
                return Err(over_cap_err());
            }
            return if buf.is_empty() {
                Ok(None)
            } else {
                let line = String::from_utf8_lossy(buf).into_owned();
                Ok(Some(line))
            };
        }
        let (take, found) = match chunk.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (chunk.len(), false),
        };
        let content = &chunk[..take - usize::from(found)];
        if !over_cap && buf.len() + content.len() > MAX_LINE_BYTES {
            if on_over_cap == OverCap::Fail {
                return Err(over_cap_err());
            }
            over_cap = true;
            buf.clear();
        }
        if !over_cap {
            buf.extend_from_slice(content);
        }
        reader.consume(take);
        if found {
            if over_cap {
                return Err(over_cap_err());
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            let line = String::from_utf8_lossy(buf).into_owned();
            return Ok(Some(line));
        }
    }
}

/// True for environment variable names that typically carry credentials
/// (`OPENAI_API_KEY`, `GITHUB_TOKEN`, `AWS_SECRET_ACCESS_KEY`, ...).
/// Plugin child processes must not inherit these by default — a plugin is
/// third-party code and the host's API keys are not its business.
fn is_sensitive_env_key(key: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "_API_KEY",
        "_ACCESS_KEY",
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_CREDENTIALS",
        "_PRIVATE_KEY",
    ];
    let upper = key.to_ascii_uppercase();
    SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
        || matches!(
            upper.as_str(),
            "API_KEY" | "TOKEN" | "SECRET" | "PASSWORD" | "CREDENTIALS"
        )
}

/// Inherited-env vars to strip from a plugin child: sensitive-looking keys
/// that the plugin manifest did NOT explicitly re-declare. (We
/// `env_remove` individual vars instead of `env_clear`ing: clearing breaks
/// process startup on Windows, where e.g. `SystemRoot` is required.)
pub(crate) fn env_vars_to_strip(
    parent: &[(String, String)],
    declared: &[(String, String)],
) -> Vec<String> {
    parent
        .iter()
        .map(|(k, _)| k)
        .filter(|k| is_sensitive_env_key(k))
        .filter(|k| !declared.iter().any(|(dk, _)| dk == *k))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn sensitive_keys_match_credentials_not_lookalikes() {
        assert!(is_sensitive_env_key("ANTHROPIC_API_KEY"));
        assert!(is_sensitive_env_key("NPM_TOKEN"));
        assert!(is_sensitive_env_key("APP_SECRET"));
        assert!(!is_sensitive_env_key("PATH"));
        assert!(!is_sensitive_env_key("TOKENIZER_THREADS")); // no _TOKEN suffix
        assert!(!is_sensitive_env_key("SECRETARY_NAME"));
    }

    #[test]
    fn strip_keeps_declared_keys() {
        let parent = vec![
            ("OPENAI_API_KEY".to_string(), "sk".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ];
        let declared = vec![("OPENAI_API_KEY".to_string(), "explicit".to_string())];
        assert!(env_vars_to_strip(&parent, &declared).is_empty());
        assert_eq!(env_vars_to_strip(&parent, &[]), vec!["OPENAI_API_KEY"]);
    }

    #[tokio::test]
    async fn read_line_bounded_caps_hostile_lines() {
        let payload = vec![b'x'; MAX_LINE_BYTES + 10];
        let mut cursor = std::io::Cursor::new(payload);
        let mut reader = tokio::io::BufReader::new(&mut cursor);
        let mut buf = Vec::new();
        let result = read_line_bounded(&mut reader, &mut buf, OverCap::Fail).await;
        assert!(result.is_err(), "over-cap line must fail");
    }

    #[tokio::test]
    async fn read_line_bounded_reads_lines_and_eof() {
        let data = b"one\ntwo\r\nthree";
        let mut cursor = std::io::Cursor::new(data.to_vec());
        let mut reader = tokio::io::BufReader::new(&mut cursor);
        let mut buf = Vec::new();
        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
                .await
                .unwrap()
                .as_deref(),
            Some("one")
        );
        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
                .await
                .unwrap()
                .as_deref(),
            Some("two")
        );
        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
                .await
                .unwrap()
                .as_deref(),
            Some("three")
        );
        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, OverCap::Fail)
                .await
                .unwrap(),
            None
        );
    }
}
