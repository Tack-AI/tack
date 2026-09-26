//! TLS for `tack serve` / `tack client`: the CBOR protocol is plaintext
//! otherwise, and the auth token (plus all session content) would be
//! sniffable off-loopback. rustls, no native TLS stack required.
//!
//! Certificates: `--tls-cert/--tls-key` for real deployments; plain `--tls`
//! generates a self-signed pair into the agent dir on first use
//! (`serve-cert.pem` / `serve-key.pem`) and reuses it. Clients verify
//! against `--tls-ca <that cert>` (TOFU by default: the generated cert is
//! offered) or `--tls-insecure` to skip verification.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};

/// (cert_pem, key_pem) paths, generating a self-signed pair on first use.
pub fn serve_cert_paths(agent_dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let cert = agent_dir.join("serve-cert.pem");
    let key = agent_dir.join("serve-key.pem");
    if cert.exists() && key.exists() {
        return Ok((cert, key));
    }
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_string(), "tack".to_string()])
            .context("cert params")?;
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        )));
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V6(
            std::net::Ipv6Addr::LOCALHOST,
        )));
    let key_pair = rcgen::KeyPair::generate().context("generate key pair")?;
    let cert_obj = params
        .self_signed(&key_pair)
        .context("generate self-signed cert")?;
    std::fs::write(&cert, cert_obj.pem()).with_context(|| format!("write {}", cert.display()))?;
    std::fs::write(&key, key_pair.serialize_pem())
        .with_context(|| format!("write {}", key.display()))?;
    // The key is sensitive: best-effort restrictive permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600));
    }
    Ok((cert, key))
}

fn load_certs(
    path: &Path,
) -> Result<Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>>> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    rustls_pemfile::certs(&mut std::io::BufReader::new(file))
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse certs from {}", path.display()))
}

fn load_key(path: &Path) -> Result<tokio_rustls::rustls::pki_types::PrivateKeyDer<'static>> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    rustls_pemfile::private_key(&mut std::io::BufReader::new(file))
        .with_context(|| format!("parse key from {}", path.display()))?
        .with_context(|| format!("no private key in {}", path.display()))
}

/// Server-side TLS acceptor from PEM paths.
pub fn acceptor(cert: &Path, key: &Path) -> Result<tokio_rustls::TlsAcceptor> {
    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(load_certs(cert)?, load_key(key)?)
        .context("build TLS server config")?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// Client-side connector. `ca`: PEM root to trust (the serve-cert.pem of a
/// self-signed server). `insecure`: skip verification entirely.
pub fn connector(ca: Option<&Path>, insecure: bool) -> Result<tokio_rustls::TlsConnector> {
    let config = if insecure {
        tokio_rustls::rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth()
    } else {
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        match ca {
            Some(ca_path) => {
                for cert in load_certs(ca_path)? {
                    roots.add(cert).context("add CA cert")?;
                }
            }
            None => {
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
        }
        tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

/// Accept-everything verifier for --tls-insecure.
#[derive(Debug)]
struct NoVerify;

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::ServerCertVerified,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        vec![
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            tokio_rustls::rustls::SignatureScheme::ED25519,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA256,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA384,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA512,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA256,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA384,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Full TLS roundtrip: generated self-signed cert, server accepts,
    /// client trusts the generated cert as CA.
    #[tokio::test]
    async fn tls_roundtrip_self_signed() {
        let tmp = tempfile::tempdir().unwrap();
        let (cert, key) = serve_cert_paths(tmp.path()).unwrap();
        let _ = &key;
        // Idempotent.
        let (cert2, _) = serve_cert_paths(tmp.path()).unwrap();
        assert_eq!(cert, cert2);

        let acceptor = super::acceptor(&cert, &key).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(stream).await.unwrap();
            use tokio::io::AsyncWriteExt as _;
            tls.write_all(b"hello-tls").await.unwrap();
        });

        let connector = super::connector(Some(&cert), false).unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut tls = connector.connect(name, stream).await.unwrap();
        use tokio::io::AsyncReadExt as _;
        let mut buf = [0u8; 9];
        tls.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello-tls");

        // Insecure connector also works.
        let acceptor = super::acceptor(&cert, &key).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = acceptor.accept(stream).await.unwrap();
        });
        let connector = super::connector(None, true).unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap();
        connector.connect(name, stream).await.unwrap();
    }
}
