//! The HTTP layer: an HTTP/1.1 client that writes requests the way
//! botocore's `URLLib3Session.send` (`botocore/httpsession.py`) and
//! `http.client` put them on the wire (`Host` and
//! `Accept-Encoding: identity` first, then the request headers in order
//! with their case), and keeps response headers in received order and
//! case. TLS uses rustls; `verify` follows `--no-verify-ssl`,
//! `--ca-bundle` / `AWS_CA_BUNDLE` / `ca_bundle`; proxies come from the
//! environment (`get_environ_proxies`); exceptions map to botocore's
//! (`EndpointConnectionError`, `ConnectTimeoutError`, `ReadTimeoutError`,
//! `ConnectionClosedError`, `SSLError`, `ProxyConnectionError`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use super::error::{ErrorKind, SessionError};
use super::url::{UrlParts, urlsplit};
use crate::protocol::HttpResponse;

/// `verify` for the HTTP session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verify {
    /// The default trust store.
    Default,
    /// `--no-verify-ssl`.
    Off,
    /// A CA bundle file (`--ca-bundle`, `AWS_CA_BUNDLE`, `ca_bundle`).
    Bundle(String),
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub verify: Verify,
    /// `None` means no timeout (`--cli-connect-timeout 0`).
    pub connect_timeout: Option<Duration>,
    /// `None` means no timeout (`--cli-read-timeout 0`).
    pub read_timeout: Option<Duration>,
    /// The process environment (proxy variables).
    pub env: HashMap<String, String>,
    /// `proxies_config.proxy_use_forwarding_for_https`.
    pub proxy_use_forwarding_for_https: bool,
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

type Conn = BufReader<Box<dyn Io>>;

pub struct HttpClient {
    config: HttpConfig,
    tls: std::sync::OnceLock<Result<Arc<rustls::ClientConfig>, SessionError>>,
    pool: Mutex<HashMap<String, Conn>>,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("config", &self.config)
            .finish()
    }
}

/// A server certificate verifier that accepts anything
/// (`--no-verify-ssl`).
#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_config(verify: &Verify) -> Result<rustls::ClientConfig, SessionError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| SessionError::new(ErrorKind::General, e.to_string()))?;
    let mut config = match verify {
        Verify::Off => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
            .with_no_client_auth(),
        Verify::Default | Verify::Bundle(_) => {
            let mut roots = rustls::RootCertStore::empty();
            match verify {
                Verify::Bundle(path) => {
                    // `SSLContext.load_verify_locations` fails inside
                    // `URLLib3Session.send`, which wraps it in
                    // `HTTPClientError`.
                    let bytes = std::fs::read(path).map_err(|e| {
                        let errno = e.raw_os_error().unwrap_or(0);
                        let text = match e.kind() {
                            std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
                            std::io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
                            _ => e.to_string(),
                        };
                        SessionError::new(
                            ErrorKind::HttpClient,
                            format!(
                                "An HTTP Client raised an unhandled exception: [Errno {errno}] {text}"
                            ),
                        )
                    })?;
                    for cert in CertificateDer::pem_slice_iter(&bytes).flatten() {
                        let _ = roots.add(cert);
                    }
                    if roots.is_empty() {
                        return Err(SessionError::new(
                            ErrorKind::HttpClient,
                            "An HTTP Client raised an unhandled exception: \
                             [X509: NO_CERTIFICATE_OR_CRL_FOUND] no certificate or crl found",
                        ));
                    }
                }
                _ => {
                    let native = rustls_native_certs::load_native_certs();
                    for cert in native.certs {
                        let _ = roots.add(cert);
                    }
                    if roots.is_empty() {
                        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                    }
                }
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// The proxy for a request, per `get_environ_proxies` /
/// `ProxyConfiguration.proxy_url_for`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub scheme: String,
    pub authorization: Option<String>,
}

/// `urllib.request.getproxies_environment`: `<scheme>_proxy` variables,
/// lowercase names taking precedence; `HTTP_PROXY` is ignored under CGI
/// (`REQUEST_METHOD` set).
fn environ_proxies(env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut proxies = HashMap::new();
    for (name, value) in env {
        let lower = name.to_lowercase();
        if !value.is_empty()
            && let Some(scheme) = lower.strip_suffix("_proxy")
        {
            proxies.insert(scheme.to_string(), value.clone());
        }
    }
    if env.contains_key("REQUEST_METHOD") {
        proxies.remove("http");
    }
    for (name, value) in env {
        if let Some(scheme) = name.strip_suffix("_proxy") {
            if name.chars().any(|c| c.is_ascii_uppercase()) {
                continue;
            }
            if value.is_empty() {
                proxies.remove(scheme);
            } else {
                proxies.insert(scheme.to_string(), value.clone());
            }
        }
    }
    proxies
}

/// `urllib.request.proxy_bypass_environment`.
fn proxy_bypass(host_port: &str, no_proxy: &str) -> bool {
    if no_proxy == "*" {
        return true;
    }
    let host = host_port.to_lowercase();
    // `_splitport`: `(.*):([0-9]*)`.
    let hostonly = match host.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
        _ => host.clone(),
    };
    for name in no_proxy.split(',') {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let name = name.trim_start_matches('.').to_lowercase();
        if hostonly == name || host == name {
            return true;
        }
        let dotted = format!(".{name}");
        if hostonly.ends_with(&dotted) || host.ends_with(&dotted) {
            return true;
        }
    }
    false
}

/// `utils.get_environ_proxies` + `ProxyConfiguration.proxy_url_for`.
pub fn proxy_for(url: &str, env: &HashMap<String, String>) -> Option<Proxy> {
    let proxies = environ_proxies(env);
    let split = urlsplit(url);
    if let Some(no_proxy) = proxies.get("no")
        && proxy_bypass(&split.netloc, no_proxy)
    {
        return None;
    }
    let raw = proxies.get(&split.scheme)?;
    // `fix_proxy_url`.
    let fixed = if raw.starts_with("http:") || raw.starts_with("https:") {
        raw.clone()
    } else if raw.starts_with("//") {
        format!("http:{raw}")
    } else {
        format!("http://{raw}")
    };
    let parts = urlsplit(&fixed);
    let (userinfo, hostport) = match parts.netloc.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_string()), h.to_string()),
        None => (None, parts.netloc.clone()),
    };
    let p = UrlParts::parse(&format!("{}://{hostport}/", parts.scheme))?;
    let authorization = userinfo.map(|u| {
        let decoded = percent_encoding::percent_decode_str(&u).decode_utf8_lossy();
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(decoded.as_bytes())
        )
    });
    Some(Proxy {
        url: fixed.clone(),
        host: p.hostname.clone(),
        port: p.effective_port(),
        scheme: parts.scheme,
        authorization,
    })
}

/// `utils.mask_proxy_url`: replace the password with `***`.
fn mask_proxy_url(url: &str) -> String {
    let parts = urlsplit(url);
    match parts.netloc.rsplit_once('@') {
        Some((userinfo, host)) => {
            let masked = match userinfo.split_once(':') {
                Some((user, _)) => format!("{user}:***"),
                None => "***".to_string(),
            };
            url.replacen(
                &format!("{userinfo}@{host}"),
                &format!("{masked}@{host}"),
                1,
            )
        }
        None => url.to_string(),
    }
}

/// An error before any response byte, on a reused connection, is retried
/// once on a new connection.
enum SendError {
    Stale,
    Fatal(SessionError),
}

impl HttpClient {
    pub fn new(config: HttpConfig) -> Result<HttpClient, SessionError> {
        Ok(HttpClient {
            config,
            tls: std::sync::OnceLock::new(),
            pool: Mutex::new(HashMap::new()),
        })
    }

    /// The TLS configuration, built at the first `https` connection (the
    /// CA bundle is loaded per connection in urllib3).
    fn tls(&self) -> Result<Arc<rustls::ClientConfig>, SessionError> {
        self.tls
            .get_or_init(|| tls_config(&self.config.verify).map(Arc::new))
            .clone()
    }

    /// Send one request and read the whole response.
    pub async fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<HttpResponse, SessionError> {
        let parts = UrlParts::parse(url).ok_or_else(|| {
            SessionError::new(ErrorKind::General, format!("Invalid endpoint: {url}"))
        })?;
        let proxy = proxy_for(url, &self.config.env);
        let key = format!(
            "{}://{}:{}|{}",
            parts.scheme,
            parts.hostname,
            parts.effective_port(),
            proxy.as_ref().map(|p| p.url.as_str()).unwrap_or("")
        );
        let pooled = self.pool.lock().unwrap().remove(&key);
        if let Some(conn) = pooled {
            match self
                .exchange(
                    conn,
                    &key,
                    method,
                    url,
                    &parts,
                    proxy.as_ref(),
                    headers,
                    body,
                )
                .await
            {
                Ok(r) => return Ok(r),
                Err(SendError::Fatal(e)) => return Err(e),
                Err(SendError::Stale) => {}
            }
        }
        let conn = self.connect(url, &parts, proxy.as_ref()).await?;
        match self
            .exchange(
                conn,
                &key,
                method,
                url,
                &parts,
                proxy.as_ref(),
                headers,
                body,
            )
            .await
        {
            Ok(r) => Ok(r),
            Err(SendError::Fatal(e)) => Err(e),
            Err(SendError::Stale) => Err(SessionError::new(
                ErrorKind::ConnectionClosed,
                format!(
                    "Connection was closed before we received a valid response from endpoint URL: \"{url}\"."
                ),
            )),
        }
    }

    async fn connect_tcp(&self, host: &str, port: u16) -> Result<TcpStream, std::io::Error> {
        let fut = TcpStream::connect((host, port));
        let stream = match self.config.connect_timeout {
            Some(t) => tokio::time::timeout(t, fut).await.map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout")
            })??,
            None => fut.await?,
        };
        let _ = stream.set_nodelay(true);
        Ok(stream)
    }

    async fn tls_wrap<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        &self,
        stream: S,
        url: &str,
        host: &str,
    ) -> Result<Box<dyn Io>, SessionError> {
        let server_name = ServerName::try_from(host.to_string()).map_err(|e| {
            SessionError::new(
                ErrorKind::Ssl,
                format!("SSL validation failed for {url} {e}"),
            )
        })?;
        let connector = tokio_rustls::TlsConnector::from(self.tls()?);
        let fut = connector.connect(server_name, stream);
        let tls = match self.config.connect_timeout {
            Some(t) => tokio::time::timeout(t, fut).await.map_err(|_| {
                SessionError::new(
                    ErrorKind::ConnectTimeout,
                    format!("Connect timeout on endpoint URL: \"{url}\""),
                )
            })?,
            None => fut.await,
        }
        .map_err(|e| {
            SessionError::new(
                ErrorKind::Ssl,
                format!("SSL validation failed for {url} {e}"),
            )
        })?;
        Ok(Box::new(tls))
    }

    async fn connect(
        &self,
        url: &str,
        parts: &UrlParts,
        proxy: Option<&Proxy>,
    ) -> Result<Conn, SessionError> {
        let https = parts.scheme == "https";
        let map_connect = |e: std::io::Error, target_url: &str, is_proxy: bool| {
            if is_proxy {
                SessionError::new(
                    ErrorKind::Proxy,
                    format!(
                        "Failed to connect to proxy URL: \"{}\"",
                        mask_proxy_url(target_url)
                    ),
                )
            } else if e.kind() == std::io::ErrorKind::TimedOut {
                SessionError::new(
                    ErrorKind::ConnectTimeout,
                    format!("Connect timeout on endpoint URL: \"{url}\""),
                )
            } else {
                SessionError::new(
                    ErrorKind::Connection,
                    format!("Could not connect to the endpoint URL: \"{url}\""),
                )
            }
        };
        match proxy {
            None => {
                let tcp = self
                    .connect_tcp(&parts.hostname, parts.effective_port())
                    .await
                    .map_err(|e| map_connect(e, url, false))?;
                let io: Box<dyn Io> = if https {
                    self.tls_wrap(tcp, url, &parts.hostname).await?
                } else {
                    Box::new(tcp)
                };
                Ok(BufReader::new(io))
            }
            Some(p) => {
                let tcp = self
                    .connect_tcp(&p.host, p.port)
                    .await
                    .map_err(|e| map_connect(e, &p.url, true))?;
                let proxy_io: Box<dyn Io> = if p.scheme == "https" {
                    self.tls_wrap(tcp, &p.url, &p.host).await?
                } else {
                    Box::new(tcp)
                };
                if !https || self.config.proxy_use_forwarding_for_https {
                    return Ok(BufReader::new(proxy_io));
                }
                // CONNECT tunnel.
                let mut conn = BufReader::new(proxy_io);
                let authority = format!(
                    "{}:{}",
                    host_for_header(&parts.hostname),
                    parts.effective_port()
                );
                let mut req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
                if let Some(auth) = &p.authorization {
                    req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
                }
                req.push_str("\r\n");
                let proxy_err = || {
                    SessionError::new(
                        ErrorKind::Proxy,
                        format!(
                            "Failed to connect to proxy URL: \"{}\"",
                            mask_proxy_url(&p.url)
                        ),
                    )
                };
                conn.get_mut()
                    .write_all(req.as_bytes())
                    .await
                    .map_err(|_| proxy_err())?;
                let (status, _, _) = read_head(&mut conn, self.config.read_timeout)
                    .await
                    .map_err(|_| proxy_err())?;
                if !(200..300).contains(&status) {
                    return Err(proxy_err());
                }
                let inner = conn.into_inner();
                let tls = self.tls_wrap(inner, url, &parts.hostname).await?;
                Ok(BufReader::new(tls))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn exchange(
        &self,
        mut conn: Conn,
        key: &str,
        method: &str,
        url: &str,
        parts: &UrlParts,
        proxy: Option<&Proxy>,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<HttpResponse, SendError> {
        let forward = proxy.is_some()
            && (parts.scheme == "http" || self.config.proxy_use_forwarding_for_https);
        let target = if forward {
            url.to_string()
        } else {
            parts.target.clone()
        };
        let mut head = format!("{method} {target} HTTP/1.1\r\n");
        let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
        if !has("host") {
            head.push_str(&format!("Host: {}\r\n", parts.host_header()));
        }
        if !has("accept-encoding") {
            head.push_str("Accept-Encoding: identity\r\n");
        }
        if forward && let Some(auth) = proxy.and_then(|p| p.authorization.as_ref()) {
            head.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
        }
        let mut msg = head.into_bytes();
        for (k, v) in headers {
            msg.extend_from_slice(k.as_bytes());
            msg.extend_from_slice(b": ");
            msg.extend_from_slice(v.as_bytes());
            msg.extend_from_slice(b"\r\n");
        }
        // `http.client` / urllib3 add Content-Length when botocore did not
        // and the method expects a body.
        if !has("content-length") && !has("transfer-encoding") {
            let expects_body = !matches!(
                method.to_uppercase().as_str(),
                "GET" | "HEAD" | "DELETE" | "OPTIONS" | "TRACE" | "CONNECT"
            );
            if !body.is_empty() || expects_body {
                msg.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
            }
        }
        msg.extend_from_slice(b"\r\n");
        msg.extend_from_slice(body);

        if conn.get_mut().write_all(&msg).await.is_err() || conn.get_mut().flush().await.is_err() {
            return Err(SendError::Stale);
        }
        let read_timeout = self.config.read_timeout;
        let closed = || {
            SendError::Fatal(SessionError::new(
                ErrorKind::ConnectionClosed,
                format!(
                    "Connection was closed before we received a valid response from endpoint URL: \"{url}\"."
                ),
            ))
        };
        let read_err = |e: HeadError| match e {
            HeadError::Timeout => SendError::Fatal(SessionError::new(
                ErrorKind::ReadTimeout,
                format!("Read timeout on endpoint URL: \"{url}\""),
            )),
            HeadError::Eof => SendError::Stale,
            HeadError::Invalid => closed(),
        };
        let (status, resp_headers) = loop {
            let (status, _, hs) = read_head(&mut conn, read_timeout).await.map_err(read_err)?;
            if status != 100 {
                break (status, hs);
            }
        };
        let get = |name: &str| {
            resp_headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        let no_body = method.eq_ignore_ascii_case("HEAD")
            || status == 204
            || status == 304
            || (100..200).contains(&status);
        let mut keep_alive = !get("connection").is_some_and(|c| c.eq_ignore_ascii_case("close"));
        let mut resp_body = Vec::new();
        let body_err = |e: std::io::Error| {
            if e.kind() == std::io::ErrorKind::TimedOut {
                SendError::Fatal(SessionError::new(
                    ErrorKind::ReadTimeout,
                    format!("Read timeout on endpoint URL: \"{url}\""),
                ))
            } else {
                closed()
            }
        };
        if !no_body {
            if get("transfer-encoding").is_some_and(|t| t.to_lowercase().contains("chunked")) {
                read_chunked(&mut conn, &mut resp_body, read_timeout)
                    .await
                    .map_err(body_err)?;
            } else if let Some(len) =
                get("content-length").and_then(|l| l.trim().parse::<usize>().ok())
            {
                resp_body.resize(len, 0);
                let mut filled = 0;
                while filled < len {
                    let n = timed(read_timeout, conn.read(&mut resp_body[filled..]))
                        .await
                        .map_err(body_err)?;
                    if n == 0 {
                        // `enforce_content_length=False`: a short body is
                        // returned as is.
                        resp_body.truncate(filled);
                        keep_alive = false;
                        break;
                    }
                    filled += n;
                }
            } else {
                keep_alive = false;
                loop {
                    let mut buf = [0u8; 65536];
                    let n = timed(read_timeout, conn.read(&mut buf))
                        .await
                        .map_err(body_err)?;
                    if n == 0 {
                        break;
                    }
                    resp_body.extend_from_slice(&buf[..n]);
                }
            }
        }
        if keep_alive {
            self.pool.lock().unwrap().insert(key.to_string(), conn);
        }
        Ok(HttpResponse {
            status,
            headers: merge_duplicate_headers(resp_headers),
            body: resp_body,
        })
    }
}

fn host_for_header(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// urllib3's `HTTPHeaderDict`: repeated headers are joined with `, `
/// under the first occurrence's name and position.
fn merge_duplicate_headers(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in headers {
        if let Some(existing) = out.iter_mut().find(|(ek, _)| ek.eq_ignore_ascii_case(&k)) {
            existing.1 = format!("{}, {v}", existing.1);
        } else {
            out.push((k, v));
        }
    }
    out
}

async fn timed<T>(
    timeout: Option<Duration>,
    fut: impl std::future::Future<Output = std::io::Result<T>>,
) -> std::io::Result<T> {
    match timeout {
        Some(t) => tokio::time::timeout(t, fut)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timeout"))?,
        None => fut.await,
    }
}

enum HeadError {
    Timeout,
    Eof,
    Invalid,
}

async fn read_line(conn: &mut Conn, timeout: Option<Duration>) -> Result<Vec<u8>, HeadError> {
    let mut line = Vec::new();
    match timed(timeout, conn.read_until(b'\n', &mut line)).await {
        Ok(0) => Err(HeadError::Eof),
        Ok(_) => Ok(line),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Err(HeadError::Timeout),
        Err(_) => Err(HeadError::Eof),
    }
}

/// Read a status line and headers (names and values as received; folded
/// continuation lines are joined with a space).
async fn read_head(
    conn: &mut Conn,
    timeout: Option<Duration>,
) -> Result<(u16, String, Vec<(String, String)>), HeadError> {
    let status_line = read_line(conn, timeout).await?;
    let status_line = String::from_utf8_lossy(&status_line).trim_end().to_string();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(HeadError::Invalid);
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or(HeadError::Invalid)?;
    let reason = parts.next().unwrap_or_default().to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let line = match read_line(conn, timeout).await {
            Ok(l) => l,
            Err(HeadError::Eof) => return Err(HeadError::Invalid),
            Err(e) => return Err(e),
        };
        let line = String::from_utf8_lossy(&line).into_owned();
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some(last) = headers.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.to_string(), v.trim().to_string()));
        }
    }
    Ok((status, reason, headers))
}

async fn read_chunked(
    conn: &mut Conn,
    out: &mut Vec<u8>,
    timeout: Option<Duration>,
) -> std::io::Result<()> {
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk");
    loop {
        let mut line = Vec::new();
        if timed(timeout, conn.read_until(b'\n', &mut line)).await? == 0 {
            return Err(invalid());
        }
        let text = String::from_utf8_lossy(&line);
        let size_str = text.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| invalid())?;
        if size == 0 {
            // Trailers until the empty line.
            loop {
                let mut t = Vec::new();
                if timed(timeout, conn.read_until(b'\n', &mut t)).await? == 0 {
                    return Ok(());
                }
                if t == b"\r\n" || t == b"\n" {
                    return Ok(());
                }
            }
        }
        let start = out.len();
        out.resize(start + size, 0);
        timed(timeout, conn.read_exact(&mut out[start..])).await?;
        let mut crlf = Vec::new();
        timed(timeout, conn.read_until(b'\n', &mut crlf)).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn proxies_from_environment() {
        let e = env(&[
            ("HTTPS_PROXY", "user:p%40ss@proxy:3128"),
            ("NO_PROXY", "localhost,.internal"),
        ]);
        let p = proxy_for("https://sts.amazonaws.com/", &e).unwrap();
        assert_eq!(p.url, "http://user:p%40ss@proxy:3128");
        assert_eq!((p.host.as_str(), p.port), ("proxy", 3128));
        assert_eq!(p.authorization.as_deref(), Some("Basic dXNlcjpwQHNz"));
        assert!(proxy_for("http://sts.amazonaws.com/", &e).is_none());
        assert!(proxy_for("https://a.internal/", &e).is_none());
        assert!(proxy_for("https://localhost:8080/", &e).is_none());
        assert_eq!(
            mask_proxy_url("http://user:secret@proxy:3128"),
            "http://user:***@proxy:3128"
        );
    }

    #[tokio::test]
    async fn wire_format_and_response_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let mut got = Vec::new();
            while !got.windows(4).any(|w| w == b"\r\n\r\n") || !got.ends_with(b"body") {
                let n = s.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            s.write_all(
                b"HTTP/1.1 200 OK\r\nX-Amzn-RequestId: r1\r\nSet-Cookie: a\r\nset-cookie: b\r\n\
                  Transfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
            )
            .await
            .unwrap();
            got
        });
        let client = HttpClient::new(HttpConfig {
            verify: Verify::Default,
            connect_timeout: Some(Duration::from_secs(5)),
            read_timeout: Some(Duration::from_secs(5)),
            env: HashMap::new(),
            proxy_use_forwarding_for_https: false,
        })
        .unwrap();
        let resp = client
            .send(
                "POST",
                &format!("http://127.0.0.1:{port}/a?b=c"),
                &[
                    ("X-Amz-Target".into(), "T".into()),
                    ("amz-sdk-request".into(), "attempt=1".into()),
                ],
                b"body",
            )
            .await
            .unwrap();
        let sent = String::from_utf8(server.await.unwrap()).unwrap();
        assert_eq!(
            sent,
            format!(
                "POST /a?b=c HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept-Encoding: identity\r\n\
                 X-Amz-Target: T\r\namz-sdk-request: attempt=1\r\nContent-Length: 4\r\n\r\nbody"
            )
        );
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"abc");
        assert_eq!(
            resp.headers,
            vec![
                ("X-Amzn-RequestId".to_string(), "r1".to_string()),
                ("Set-Cookie".to_string(), "a, b".to_string()),
                ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn missing_ca_bundle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _conn = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let client = HttpClient::new(HttpConfig {
            verify: Verify::Bundle("/nonexistent/ca.pem".into()),
            connect_timeout: Some(Duration::from_secs(5)),
            read_timeout: Some(Duration::from_secs(5)),
            env: HashMap::new(),
            proxy_use_forwarding_for_https: false,
        })
        .unwrap();
        let err = client
            .send("GET", &format!("https://127.0.0.1:{port}/"), &[], b"")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::HttpClient);
        assert_eq!(
            err.message,
            "An HTTP Client raised an unhandled exception: [Errno 2] No such file or directory"
        );
    }

    #[tokio::test]
    async fn connection_refused_message() {
        let client = HttpClient::new(HttpConfig {
            verify: Verify::Off,
            connect_timeout: Some(Duration::from_secs(5)),
            read_timeout: None,
            env: HashMap::new(),
            proxy_use_forwarding_for_https: false,
        })
        .unwrap();
        let err = client
            .send("GET", "http://127.0.0.1:9/x?y=1", &[], b"")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Connection);
        assert_eq!(
            err.message,
            "Could not connect to the endpoint URL: \"http://127.0.0.1:9/x?y=1\""
        );
    }
}
