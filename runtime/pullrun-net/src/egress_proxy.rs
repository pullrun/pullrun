// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Credential-injecting egress proxy (HTTPS MITM).
//!
//! Workloads are configured to use this proxy and trust a per-runtime CA.
//! On CONNECT the proxy terminates TLS with a certificate for the target
//! host, injects the configured credential header, and re-originates TLS
//! to the real server. The sandbox never holds the secret.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// One credential injection rule.
#[derive(Debug, Clone)]
pub struct CredentialRule {
    /// Exact hostname, e.g. `api.openai.com`.
    pub host: String,
    /// Header to inject, e.g. `authorization`.
    pub header: String,
    /// Secret name resolved through the provider.
    pub secret: String,
}

/// Secret lookup callback. Returns the plaintext secret.
pub type SecretProvider = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

struct HostCertResolver {
    ca: Arc<Ca>,
}

impl std::fmt::Debug for HostCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostCertResolver")
    }
}

impl ResolvesServerCert for HostCertResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = hello.server_name()?.to_string();
        match self.ca.leaf(&host) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(%host, error = %e, "egress proxy: could not mint leaf cert");
                None
            }
        }
    }
}

/// Runtime CA and per-host leaf certificate cache.
pub struct Ca {
    cert: Certificate,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for Ca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ca")
    }
}

impl Ca {
    pub fn generate() -> Result<Self, String> {
        let mut params = CertificateParams::new(Vec::new());
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "Pullrun Egress CA");
        let cert = Certificate::from_params(params).map_err(|e| e.to_string())?;
        Ok(Self {
            cert,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// PEM-encoded CA certificate, for mounting into sandboxes.
    pub fn pem(&self) -> String {
        self.cert.serialize_pem().unwrap_or_default()
    }

    fn leaf(&self, host: &str) -> Result<Arc<CertifiedKey>, String> {
        if let Some(c) = self.cache.lock().expect("ca cache poisoned").get(host) {
            return Ok(Arc::clone(c));
        }
        let params = CertificateParams::new(vec![host.to_string()]);
        let leaf = Certificate::from_params(params).map_err(|e| e.to_string())?;
        let cert_der = CertificateDer::from(
            leaf.serialize_der_with_signer(&self.cert)
                .map_err(|e| e.to_string())?,
        );
        let key_der =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.serialize_private_key_der()));
        let signing = rustls::crypto::ring::sign::any_supported_type(&key_der)
            .map_err(|e| format!("leaf signing key: {e}"))?;
        let certified = Arc::new(CertifiedKey::new(vec![cert_der], signing));
        self.cache
            .lock()
            .expect("ca cache poisoned")
            .insert(host.to_string(), Arc::clone(&certified));
        Ok(certified)
    }
}

/// Credential-injecting proxy.
#[derive(Clone)]
pub struct CredentialProxy {
    ca: Arc<Ca>,
    rules: Arc<Vec<CredentialRule>>,
    secrets: SecretProvider,
    server_config: Arc<rustls::ServerConfig>,
}

impl CredentialProxy {
    pub fn new(rules: Vec<CredentialRule>, secrets: SecretProvider) -> Result<Self, String> {
        let ca = Arc::new(Ca::generate()?);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(HostCertResolver {
                ca: Arc::clone(&ca),
            }));
        Ok(Self {
            ca,
            rules: Arc::new(rules),
            secrets,
            server_config: Arc::new(server_config),
        })
    }

    pub fn ca_pem(&self) -> String {
        self.ca.pem()
    }

    fn rule_for(&self, host: &str) -> Option<&CredentialRule> {
        self.rules.iter().find(|r| r.host == host)
    }

    pub async fn serve(self, listener: TcpListener) -> std::io::Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            let proxy = self.clone();
            tokio::spawn(async move {
                if let Err(e) = proxy.handle(stream).await {
                    tracing::debug!(%peer, error = %e, "egress proxy connection ended");
                }
            });
        }
    }

    async fn handle(&self, mut stream: TcpStream) -> Result<(), String> {
        let mut reader = BufReader::new(&mut stream);
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .await
            .map_err(|e| e.to_string())?;
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .map_err(|e| e.to_string())?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
            headers.push(line.trim_end().to_string());
        }

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 3 {
            return Err("malformed proxy request".into());
        }
        if parts[0] != "CONNECT" {
            let _ = stream
                .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n")
                .await;
            return Ok(());
        }
        let authority = parts[1];
        let host = authority
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority);
        let rule = match self.rule_for(host) {
            Some(r) => r.clone(),
            None => {
                let _ = stream
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
                    .await;
                return Ok(());
            }
        };
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .map_err(|e| e.to_string())?;

        let acceptor = TlsAcceptor::from(Arc::clone(&self.server_config));
        let mut tls = acceptor
            .accept(stream)
            .await
            .map_err(|e| format!("proxy TLS handshake: {e}"))?;

        // Read one HTTP request over the terminated TLS connection.
        let mut tls_reader = BufReader::new(&mut tls);
        let mut req_line = String::new();
        tls_reader
            .read_line(&mut req_line)
            .await
            .map_err(|e| e.to_string())?;
        if req_line.is_empty() {
            return Ok(());
        }
        let mut req_headers = Vec::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            tls_reader
                .read_line(&mut line)
                .await
                .map_err(|e| e.to_string())?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(v) = line
                .to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
            {
                content_length = v;
            }
            req_headers.push(line.trim_end().to_string());
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            tls_reader
                .read_exact(&mut body)
                .await
                .map_err(|e| e.to_string())?;
        }

        // Connect upstream with real TLS.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_config));
        let port = authority
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .unwrap_or(443);
        let upstream_tcp = TcpStream::connect((host, port))
            .await
            .map_err(|e| format!("connect {host}:{port}: {e}"))?;
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| format!("server name {host}: {e}"))?;
        let mut upstream = connector
            .connect(server_name, upstream_tcp)
            .await
            .map_err(|e| format!("upstream TLS: {e}"))?;

        let secret = (self.secrets)(&rule.secret)
            .ok_or_else(|| format!("secret {} not found", rule.secret))?;
        let mut out = String::new();
        out.push_str(&req_line);
        for h in &req_headers {
            let lower = h.to_ascii_lowercase();
            if lower.starts_with("proxy-connection:")
                || lower.starts_with("connection:")
                || lower.starts_with(&format!("{}:", rule.header.to_ascii_lowercase()))
            {
                continue;
            }
            out.push_str(h);
            out.push_str("\r\n");
        }
        out.push_str(&format!("{}: {}\r\n", rule.header, secret));
        out.push_str("Connection: close\r\n\r\n");
        upstream
            .write_all(out.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        upstream.write_all(&body).await.map_err(|e| e.to_string())?;
        upstream.flush().await.map_err(|e| e.to_string())?;

        let mut resp_reader = BufReader::new(&mut upstream);
        let mut status = String::new();
        resp_reader
            .read_line(&mut status)
            .await
            .map_err(|e| e.to_string())?;
        let mut resp_headers = Vec::new();
        loop {
            let mut line = String::new();
            resp_reader
                .read_line(&mut line)
                .await
                .map_err(|e| e.to_string())?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
            resp_headers.push(line);
        }
        let mut response = status;
        for h in resp_headers {
            response.push_str(&h);
        }
        response.push_str("\r\n");
        tls.write_all(response.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        tokio::io::copy(&mut resp_reader, &mut tls)
            .await
            .map_err(|e| e.to_string())?;
        tls.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Bind a credential proxy listener.
pub async fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}
