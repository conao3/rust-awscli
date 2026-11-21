//! `urllib.request` as the agent-registry customization opens URLs
//! (`agentregistry/utils.py` `urlopen`, `_opener`, `_RefuseRedirects`,
//! `_ssl_context`): `https` only, a verifying TLS context that trusts the
//! CLI's CA bundle, the environment's proxies (a `CONNECT` tunnel), no
//! redirects, and the request and errors of CPython 3.14's `urllib`
//! (`HTTPError`, `URLError`, and the `OSError`s of a read).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};

use super::utils::{printable_url, require_https};

/// `http.client.HTTPConnection.default_port` of `https`.
const HTTPS_PORT: u16 = 443;

/// `OpenerDirector.addheaders` (`'Python-urllib/%s' % __version__`).
const USER_AGENT: &str = "Python-urllib/3.14";

/// The exceptions `urlopen` and reading its response raise.
#[derive(Debug)]
pub(crate) enum UrlError {
    /// `urllib.error.HTTPError`.
    Http(HttpError),
    /// `urllib.error.URLError(reason)`: `str(reason)`, and whether the
    /// reason is an `ssl.SSLCertVerificationError`.
    Url { reason: String, cert: bool },
    /// An `OSError` or `http.client.HTTPException` raised while the
    /// response is read: `str(e)` and `type(e).__name__`.
    Os { message: String, name: &'static str },
    /// `AgentRegistryJWTError` (the scheme check, a redirect, a response
    /// too large).
    Jwt(String),
}

impl UrlError {
    /// `str(e)`.
    pub(crate) fn message(&self) -> String {
        match self {
            UrlError::Http(e) => format!("HTTP Error {}: {}", e.code, e.reason),
            UrlError::Url { reason, .. } => format!("<urlopen error {reason}>"),
            UrlError::Os { message, .. } => message.clone(),
            UrlError::Jwt(m) => m.clone(),
        }
    }

    /// `type(e).__name__`.
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            UrlError::Http(_) => "HTTPError",
            UrlError::Url { .. } => "URLError",
            UrlError::Os { name, .. } => name,
            UrlError::Jwt(_) => "AgentRegistryJWTError",
        }
    }
}

/// `urllib.error.HTTPError`: the status, its reason phrase, and the body
/// (read when the error is raised; `None` when reading it failed).
#[derive(Debug)]
pub(crate) struct HttpError {
    pub code: u16,
    pub reason: String,
    body: Option<Vec<u8>>,
}

impl HttpError {
    /// `agentregistry.utils.read_error_body`: at most `limit` bytes of the
    /// body as text, or `''` when it could not be read.
    pub(crate) fn read_error_body(&self, limit: usize) -> String {
        match &self.body {
            Some(b) => String::from_utf8_lossy(&b[..b.len().min(limit)]).into_owned(),
            None => {
                crate::debuglog::debug(
                    "awscli.customizations.agentregistry.utils",
                    &format!(
                        "Could not read the body of an HTTP {} response: timed out",
                        self.code
                    ),
                );
                String::new()
            }
        }
    }
}

/// The most of an error body kept (the largest `read_error_body` limit).
const ERROR_BODY_CAP: usize = 64 * 1024;

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

enum Framing {
    Length(usize),
    Chunked { left: usize, done: bool },
    UntilClose,
}

/// An open response (`http.client.HTTPResponse`).
pub(crate) struct Response {
    reader: BufReader<Box<dyn Stream>>,
    framing: Framing,
}

fn read_error(e: std::io::Error) -> UrlError {
    if matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        UrlError::Os {
            message: "The read operation timed out".to_string(),
            name: "TimeoutError",
        }
    } else {
        UrlError::Os {
            message: os_text(&e),
            name: "OSError",
        }
    }
}

fn incomplete(read: usize, expected: Option<usize>) -> UrlError {
    UrlError::Os {
        message: match expected {
            Some(n) => format!("IncompleteRead({read} bytes read, {n} more expected)"),
            None => format!("IncompleteRead({read} bytes read)"),
        },
        name: "IncompleteRead",
    }
}

impl Response {
    /// `HTTPResponse.read(amt)`: up to `amt` bytes, fewer only at the end
    /// of the body.
    pub(crate) fn read(&mut self, amt: usize) -> Result<Vec<u8>, UrlError> {
        let mut out = Vec::new();
        while out.len() < amt {
            let want = amt - out.len();
            match &mut self.framing {
                Framing::Length(left) => {
                    if *left == 0 {
                        break;
                    }
                    let mut buf = vec![0u8; want.min(*left).min(65536)];
                    let n = self.reader.read(&mut buf).map_err(read_error)?;
                    if n == 0 {
                        return Err(incomplete(out.len(), Some(*left)));
                    }
                    *left -= n;
                    out.extend_from_slice(&buf[..n]);
                }
                Framing::Chunked { left, done } => {
                    if *done {
                        break;
                    }
                    if *left == 0 {
                        let mut line = String::new();
                        self.reader.read_line(&mut line).map_err(read_error)?;
                        let size = line.split(';').next().unwrap_or_default().trim();
                        let size = usize::from_str_radix(size, 16).map_err(|_| UrlError::Os {
                            message: "IncompleteRead(0 bytes read)".to_string(),
                            name: "IncompleteRead",
                        })?;
                        if size == 0 {
                            // The trailer, up to the empty line.
                            loop {
                                let mut t = String::new();
                                let n = self.reader.read_line(&mut t).map_err(read_error)?;
                                if n == 0 || t == "\r\n" || t == "\n" {
                                    break;
                                }
                            }
                            *done = true;
                            continue;
                        }
                        *left = size;
                    }
                    let mut buf = vec![0u8; want.min(*left).min(65536)];
                    let n = self.reader.read(&mut buf).map_err(read_error)?;
                    if n == 0 {
                        return Err(incomplete(out.len(), None));
                    }
                    *left -= n;
                    out.extend_from_slice(&buf[..n]);
                    if *left == 0 {
                        let mut crlf = String::new();
                        self.reader.read_line(&mut crlf).map_err(read_error)?;
                    }
                }
                Framing::UntilClose => {
                    let mut buf = vec![0u8; want.min(65536)];
                    let n = match self.reader.read(&mut buf) {
                        Ok(n) => n,
                        // A TLS peer that closes without `close_notify`.
                        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
                        Err(e) => return Err(read_error(e)),
                    };
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
            }
        }
        Ok(out)
    }
}

/// `str(OSError)`: `[Errno N] text`.
pub(crate) fn os_text(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => {
            let text = std::io::Error::from_raw_os_error(code).to_string();
            let text = text
                .split(" (os error")
                .next()
                .unwrap_or_default()
                .to_string();
            format!("[Errno {code}] {text}")
        }
        None => e.to_string(),
    }
}

/// `socket.gaierror` for a failed name lookup.
fn gai_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = text
        .strip_prefix("failed to lookup address information: ")
        .unwrap_or(&text)
        .to_string();
    let errno = match text.as_str() {
        "Name or service not known" => -2,
        "Temporary failure in name resolution" => -3,
        "No address associated with hostname" => -5,
        "Servname not supported for ai_socktype" => -8,
        _ => -2,
    };
    format!("[Errno {errno}] {text}")
}

/// `_ssl_context(ca_bundle)`: the roots of `ca_bundle` (a file, or a
/// directory of files), or the default trust store. `Err` is `str(e)` of
/// the `OSError` the load raised.
fn tls_config(ca_bundle: Option<&str>) -> Result<Arc<rustls::ClientConfig>, (String, String)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    match ca_bundle {
        Some(path) => {
            let fail = |e: &std::io::Error| (path.to_string(), os_text(e));
            let mut files = Vec::new();
            if std::path::Path::new(path).is_dir() {
                for entry in std::fs::read_dir(path).map_err(|e| fail(&e))?.flatten() {
                    files.push(entry.path());
                }
            } else {
                files.push(std::path::PathBuf::from(path));
            }
            for f in &files {
                let bytes = match std::fs::read(f) {
                    Ok(b) => b,
                    Err(e) if files.len() == 1 => return Err(fail(&e)),
                    Err(_) => continue,
                };
                for cert in CertificateDer::pem_slice_iter(&bytes).flatten() {
                    let _ = roots.add(cert);
                }
            }
            if roots.is_empty() && !std::path::Path::new(path).is_dir() {
                return Err((
                    path.to_string(),
                    "[X509: NO_CERTIFICATE_OR_CRL_FOUND] no certificate or crl found (_ssl.c:4489)"
                        .to_string(),
                ));
            }
        }
        None => {
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
        }
    }
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| (String::new(), e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// `rustls`'s verification failure as `ssl.SSLCertVerificationError`
/// words it.
fn cert_error_text(e: &rustls::Error) -> String {
    let detail = match e {
        rustls::Error::InvalidCertificate(c) => match c {
            rustls::CertificateError::UnknownIssuer => "unable to get local issuer certificate",
            rustls::CertificateError::Expired => "certificate has expired",
            rustls::CertificateError::NotValidYet => "certificate is not yet valid",
            rustls::CertificateError::NotValidForName
            | rustls::CertificateError::NotValidForNameContext { .. } => "Hostname mismatch",
            _ => "certificate verify failed",
        },
        _ => "certificate verify failed",
    };
    format!("[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed: {detail} (_ssl.c:1085)")
}

/// A request: `urllib.request.Request(url, data=..., headers=...)`.
pub(crate) struct Request<'a> {
    pub url: &'a str,
    /// `data`: a POST with this body.
    pub data: Option<&'a [u8]>,
    /// `headers` (a `Content-Type` here).
    pub headers: &'a [(&'a str, &'a str)],
}

/// `agentregistry.utils.urlopen(request, timeout, purpose, ca_bundle)`.
pub(crate) fn urlopen(
    request: &Request<'_>,
    timeout: Duration,
    purpose: &str,
    ca_bundle: Option<&str>,
) -> Result<Response, UrlError> {
    require_https(request.url, purpose).map_err(UrlError::Jwt)?;
    // `_opener` builds the TLS context first.
    let tls = tls_config(ca_bundle)
        .map_err(|(path, e)| UrlError::Jwt(format!("Could not load the CA bundle {path}: {e}")))?;
    let split = crate::endpoint::pyurl::urlsplit_parts(request.url).map_err(|e| UrlError::Url {
        reason: e.0,
        cert: false,
    })?;
    // `Request._parse`: the host is the netloc without user info.
    let host_header = split
        .netloc
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .to_string();
    let hostname = split.hostname().unwrap_or_default();
    let port = split.port().ok().flatten().unwrap_or(HTTPS_PORT);
    let mut selector = split.path.clone();
    if selector.is_empty() {
        selector.push('/');
    }
    if !split.query.is_empty() {
        selector = format!("{selector}?{}", split.query);
    }

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let proxy = crate::session::http::proxy_for(request.url, &env);
    let (connect_host, connect_port) = match &proxy {
        Some(p) => (p.host.clone(), p.port),
        None => (hostname.clone(), port),
    };
    let tcp = connect(&connect_host, connect_port, timeout)?;
    let mut stream: Box<dyn Stream> = Box::new(tcp);
    if let Some(p) = &proxy {
        // `HTTPConnection._tunnel`.
        let authority = format!("{}:{port}", host_for_tunnel(&hostname));
        let mut req = format!("CONNECT {authority} HTTP/1.1\r\n");
        if let Some(auth) = &p.authorization {
            req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
        }
        req.push_str(&format!("Host: {authority}\r\n\r\n"));
        let os = |e: std::io::Error| UrlError::Url {
            reason: os_text(&e),
            cert: false,
        };
        stream.write_all(req.as_bytes()).map_err(os)?;
        let mut reader = BufReader::new(stream);
        let (status, reason, _) = read_head(&mut reader).map_err(|e| match e {
            Head::Io(e) => os(e),
            Head::Invalid(line) => UrlError::Url {
                reason: format!("Remote end closed connection without response{line}"),
                cert: false,
            },
        })?;
        if status != 200 {
            return Err(UrlError::Url {
                reason: format!("Tunnel connection failed: {status} {reason}"),
                cert: false,
            });
        }
        stream = Box::new(UnbufferedAfter(reader));
    }
    let server_name = ServerName::try_from(hostname.clone()).map_err(|e| UrlError::Url {
        reason: e.to_string(),
        cert: false,
    })?;
    let conn = rustls::ClientConnection::new(tls, server_name).map_err(|e| UrlError::Url {
        reason: e.to_string(),
        cert: false,
    })?;
    let mut tls_stream = rustls::StreamOwned::new(conn, stream);
    // The handshake happens in `HTTPConnection.connect` (`wrap_socket`).
    while tls_stream.conn.is_handshaking() {
        if let Err(e) = tls_stream.conn.complete_io(&mut tls_stream.sock) {
            let cert = e
                .get_ref()
                .and_then(|i| i.downcast_ref::<rustls::Error>())
                .filter(|r| matches!(r, rustls::Error::InvalidCertificate(_)))
                .map(cert_error_text);
            return Err(match cert {
                Some(reason) => UrlError::Url { reason, cert: true },
                None if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    UrlError::Url {
                        reason: "_ssl.c:1011: The handshake operation timed out".to_string(),
                        cert: false,
                    }
                }
                None => UrlError::Url {
                    reason: match e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
                        Some(r) => format!("[SSL] {r} (_ssl.c:1085)"),
                        None => os_text(&e),
                    },
                    cert: false,
                },
            });
        }
    }

    // `AbstractHTTPHandler.do_request_` and `do_open`: the headers after
    // `Accept-Encoding`, title-cased, `Connection: close` last.
    let mut head = format!(
        "{} {selector} HTTP/1.1\r\nAccept-Encoding: identity\r\n",
        if request.data.is_some() {
            "POST"
        } else {
            "GET"
        }
    );
    if let Some(data) = request.data {
        if !request
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            head.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
        }
        head.push_str(&format!("Content-Length: {}\r\n", data.len()));
    }
    head.push_str(&format!(
        "Host: {host_header}\r\nUser-Agent: {USER_AGENT}\r\n"
    ));
    for (k, v) in request.headers {
        head.push_str(&format!("{}: {v}\r\n", title(k)));
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut message = head.into_bytes();
    if let Some(data) = request.data {
        message.extend_from_slice(data);
    }
    let send = |e: std::io::Error| UrlError::Url {
        reason: os_text(&e),
        cert: false,
    };
    tls_stream.write_all(&message).map_err(send)?;
    tls_stream.flush().map_err(send)?;

    let mut reader: BufReader<Box<dyn Stream>> = BufReader::new(Box::new(tls_stream));
    let (status, reason, headers) = loop {
        let (status, reason, headers) = read_head(&mut reader).map_err(|e| match e {
            Head::Io(e) => read_error(e),
            Head::Invalid(_) => UrlError::Os {
                message: "Remote end closed connection without response".to_string(),
                name: "RemoteDisconnected",
            },
        })?;
        if status != 100 {
            break (status, reason, headers);
        }
    };
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let framing = if status == 204 || status == 304 || (100..200).contains(&status) {
        Framing::Length(0)
    } else if get("transfer-encoding").is_some_and(|t| t.to_lowercase().contains("chunked")) {
        Framing::Chunked {
            left: 0,
            done: false,
        }
    } else if let Some(n) = get("content-length").and_then(|l| l.trim().parse::<usize>().ok()) {
        Framing::Length(n)
    } else {
        Framing::UntilClose
    };
    let mut response = Response { reader, framing };
    if (200..300).contains(&status) {
        return Ok(response);
    }
    // `HTTPRedirectHandler.http_error_302`.
    if matches!(status, 301 | 302 | 303 | 307 | 308)
        && let Some(location) = get("location").or_else(|| get("uri"))
    {
        let target = crate::endpoint::pyurl::urlsplit_parts(&location)
            .map(|p| p.scheme)
            .unwrap_or_default();
        if !matches!(target.as_str(), "http" | "https" | "ftp" | "") {
            return Err(UrlError::Http(HttpError {
                code: status,
                reason: format!("{reason} - Redirection to url '{location}' is not allowed"),
                body: response.read(ERROR_BODY_CAP).ok(),
            }));
        }
        return Err(UrlError::Jwt(format!(
            "Refusing to follow a redirect (HTTP {status}) from {}.",
            printable_url(request.url)
        )));
    }
    let body = response.read(ERROR_BODY_CAP).ok();
    Err(UrlError::Http(HttpError {
        code: status,
        reason,
        body,
    }))
}

/// A `BufReader` that hands out what it buffered before reading on.
struct UnbufferedAfter(BufReader<Box<dyn Stream>>);

impl Read for UnbufferedAfter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for UnbufferedAfter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.get_mut().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.get_mut().flush()
    }
}

fn host_for_tunnel(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// `str.title()` of a header name.
fn title(name: &str) -> String {
    let mut out = String::new();
    let mut start = true;
    for c in name.chars() {
        if c.is_alphabetic() {
            if start {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            start = false;
        } else {
            out.push(c);
            start = true;
        }
    }
    out
}

/// `socket.create_connection((host, port), timeout)`.
fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, UrlError> {
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| UrlError::Url {
            reason: gai_text(&e),
            cert: false,
        })?
        .collect();
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(timeout));
                let _ = s.set_write_timeout(Some(timeout));
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    let reason = match last {
        Some(e) if e.kind() == std::io::ErrorKind::TimedOut => "timed out".to_string(),
        Some(e) => os_text(&e),
        None => "getaddrinfo returns an empty list".to_string(),
    };
    Err(UrlError::Url {
        reason,
        cert: false,
    })
}

enum Head {
    Io(std::io::Error),
    /// The status line was missing or malformed.
    Invalid(String),
}

type StatusHead = (u16, String, Vec<(String, String)>);

fn read_head<R: BufRead>(reader: &mut R) -> Result<StatusHead, Head> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).map_err(Head::Io)?;
    if n == 0 {
        return Err(Head::Invalid(String::new()));
    }
    let status_line = line.trim_end_matches(['\r', '\n']);
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let status = parts.next().and_then(|s| s.parse::<u16>().ok());
    let Some(status) = status.filter(|_| version.starts_with("HTTP/")) else {
        return Err(Head::Invalid(String::new()));
    };
    let reason = parts.next().unwrap_or_default().trim().to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h).map_err(Head::Io)?;
        let h = h.trim_end_matches(['\r', '\n']);
        if n == 0 || h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.to_string(), v.trim().to_string()));
        }
    }
    Ok((status, reason, headers))
}
