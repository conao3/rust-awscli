//! The parts of Python's `urllib.parse` and botocore's URL helpers that the
//! endpoint standard library depends on (`urllib/parse.py` of CPython 3.14,
//! `botocore/utils.py`).

use std::net::IpAddr;
use std::sync::OnceLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

/// The result of `urllib.parse.urlparse`.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseResult {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub params: String,
    pub query: String,
    pub fragment: String,
}

/// A `ValueError` raised by `urlsplit` / `urlparse`.
#[derive(Debug, Clone, PartialEq)]
pub struct UrlError(pub String);

const SCHEME_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+-.";

const USES_PARAMS: [&str; 16] = [
    "", "ftp", "hdl", "prospero", "http", "imap", "https", "shttp", "rtsp", "rtsps", "rtspu",
    "sip", "sips", "mms", "sftp", "tel",
];

fn is_c0_or_space(c: char) -> bool {
    c <= ' '
}

/// `urllib.parse._urlsplit`.
fn urlsplit(url: &str) -> Result<(String, Option<String>, String, String, String), UrlError> {
    let url: String = url
        .trim_start_matches(is_c0_or_space)
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut scheme = String::new();
    let mut rest = url.as_str();
    if let Some(i) = rest.find(':')
        && i > 0
    {
        let first = rest.chars().next().unwrap();
        if first.is_ascii_alphabetic() && rest[..i].chars().all(|c| SCHEME_CHARS.contains(c)) {
            scheme = rest[..i].to_lowercase();
            rest = &rest[i + 1..];
        }
    }
    let mut netloc = None;
    let mut rest = rest.to_string();
    if rest.starts_with("//") {
        let after = &rest[2..];
        let delim = after.find(['/', '?', '#']).unwrap_or(after.len());
        let n = after[..delim].to_string();
        rest = after[delim..].to_string();
        if (n.contains('[') && !n.contains(']')) || (n.contains(']') && !n.contains('[')) {
            return Err(UrlError("Invalid IPv6 URL".into()));
        }
        if n.contains('[') && n.contains(']') {
            check_bracketed_netloc(&n)?;
        }
        netloc = Some(n);
    }
    let mut fragment = String::new();
    if let Some((u, f)) = rest.split_once('#') {
        fragment = f.to_string();
        rest = u.to_string();
    }
    let mut query = String::new();
    if let Some((u, q)) = rest.split_once('?') {
        query = q.to_string();
        rest = u.to_string();
    }
    if let Some(n) = &netloc {
        check_netloc(n)?;
    }
    Ok((scheme, netloc, rest, query, fragment))
}

/// `urllib.parse._check_bracketed_netloc`.
fn check_bracketed_netloc(netloc: &str) -> Result<(), UrlError> {
    let host_and_port = rpartition(netloc, '@').2;
    let (before, have_open, bracketed) = partition(host_and_port, '[');
    let hostname = if have_open {
        if !before.is_empty() {
            return Err(UrlError("Invalid IPv6 URL".into()));
        }
        let (hostname, _, port) = partition(bracketed, ']');
        if !port.is_empty() && !port.starts_with(':') {
            return Err(UrlError("Invalid IPv6 URL".into()));
        }
        hostname
    } else {
        partition(host_and_port, ':').0
    };
    check_bracketed_host(hostname)
}

/// `urllib.parse._check_bracketed_host` (`ipaddress.ip_address` accepts an
/// IPv6 scope id after `%`).
fn check_bracketed_host(hostname: &str) -> Result<(), UrlError> {
    if hostname.starts_with(['v', 'V']) {
        static FUTURE: OnceLock<Regex> = OnceLock::new();
        let re = FUTURE.get_or_init(|| Regex::new(r"(?s)\A[vV][a-fA-F0-9]+\..+\z").unwrap());
        if !re.is_match(hostname) {
            return Err(UrlError("IPvFuture address is invalid".into()));
        }
        return Ok(());
    }
    let invalid = || {
        UrlError(format!(
            "{} does not appear to be an IPv4 or IPv6 address",
            crate::pyfmt::str_repr(hostname)
        ))
    };
    if let Ok(IpAddr::V4(_)) = hostname.parse::<IpAddr>() {
        return Err(UrlError("An IPv4 address cannot be in brackets".into()));
    }
    let (addr, has_scope, scope) = partition(hostname, '%');
    if has_scope && (scope.is_empty() || scope.contains('%')) {
        return Err(invalid());
    }
    match addr.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => Ok(()),
        _ => Err(invalid()),
    }
}

/// `urllib.parse._checknetloc`.
fn check_netloc(netloc: &str) -> Result<(), UrlError> {
    if netloc.is_empty() || netloc.is_ascii() {
        return Ok(());
    }
    let n: String = netloc
        .chars()
        .filter(|c| !matches!(c, '@' | ':' | '#' | '?'))
        .collect();
    let normalized: String = n.nfkc().collect();
    if n == normalized {
        return Ok(());
    }
    if normalized.contains(['/', '?', '#', '@', ':']) {
        return Err(UrlError(format!(
            "netloc '{netloc}' contains invalid characters under NFKC normalization"
        )));
    }
    Ok(())
}

/// `urllib.parse.urlparse(url)`.
pub fn urlparse(url: &str) -> Result<ParseResult, UrlError> {
    let (scheme, netloc, mut path, query, fragment) = urlsplit(url)?;
    let mut params = String::new();
    if USES_PARAMS.contains(&scheme.as_str()) && path.contains(';') {
        let (p, rest) = split_params(&path);
        params = rest.unwrap_or_default();
        path = p;
    }
    Ok(ParseResult {
        scheme,
        netloc: netloc.unwrap_or_default(),
        path,
        params,
        query,
        fragment,
    })
}

/// `urllib.parse._splitparams(url, allow_none=True)`.
fn split_params(url: &str) -> (String, Option<String>) {
    let i = if let Some(slash) = url.rfind('/') {
        match url[slash..].find(';') {
            Some(i) => slash + i,
            None => return (url.to_string(), None),
        }
    } else {
        url.find(';').unwrap()
    };
    (url[..i].to_string(), Some(url[i + 1..].to_string()))
}

impl ParseResult {
    /// `_NetlocResultMixinStr._hostinfo`.
    fn hostinfo(&self) -> (&str, Option<&str>) {
        let hostinfo = rpartition(&self.netloc, '@').2;
        let (_, have_open, bracketed) = partition(hostinfo, '[');
        let (hostname, port) = if have_open {
            let (hostname, _, port) = partition(bracketed, ']');
            (hostname, partition(port, ':').2)
        } else {
            let (hostname, _, port) = partition(hostinfo, ':');
            (hostname, port)
        };
        (hostname, (!port.is_empty()).then_some(port))
    }

    /// `_NetlocResultMixinBase.hostname`.
    pub fn hostname(&self) -> Option<String> {
        let hostname = self.hostinfo().0;
        if hostname.is_empty() {
            return None;
        }
        let (host, percent, zone) = partition(hostname, '%');
        Some(format!(
            "{}{}{}",
            py_lower(host),
            if percent { "%" } else { "" },
            zone
        ))
    }

    /// `_NetlocResultMixinBase.port`; `Err` is the `ValueError`.
    pub fn port(&self) -> Result<Option<u16>, UrlError> {
        match self.hostinfo().1 {
            None => Ok(None),
            Some(port) => {
                if !port.chars().all(|c| c.is_ascii_digit()) {
                    return Err(UrlError(format!(
                        "Port could not be cast to integer value as {}",
                        crate::pyfmt::str_repr(port)
                    )));
                }
                let trimmed = port.trim_start_matches('0');
                if trimmed.len() > 5 {
                    return Err(UrlError("Port out of range 0-65535".into()));
                }
                let n: u32 = if trimmed.is_empty() {
                    0
                } else {
                    trimmed.parse().unwrap()
                };
                u16::try_from(n)
                    .map(Some)
                    .map_err(|_| UrlError("Port out of range 0-65535".into()))
            }
        }
    }
}

/// Python `str.lower()`.
pub fn py_lower(s: &str) -> String {
    s.to_lowercase()
}

/// Python `str.partition(sep)`.
pub fn partition(s: &str, sep: char) -> (&str, bool, &str) {
    match s.split_once(sep) {
        Some((a, b)) => (a, true, b),
        None => (s, false, ""),
    }
}

/// Python `str.rpartition(sep)`.
pub fn rpartition(s: &str, sep: char) -> (&str, bool, &str) {
    match s.rsplit_once(sep) {
        Some((a, b)) => (a, true, b),
        None => ("", false, s),
    }
}

/// `urllib.parse.quote(string, safe)`: UTF-8 encode, keep ASCII letters,
/// digits, `_.-~` and the `safe` characters, and percent-encode the rest
/// with uppercase hex.
pub fn quote(s: &str, safe: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if b.is_ascii_alphanumeric() || "_.-~".contains(c) || (b.is_ascii() && safe.contains(c)) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// botocore `utils.normalize_url_path`.
pub fn normalize_url_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    remove_dot_segments(path)
}

/// botocore `utils.remove_dot_segments`.
fn remove_dot_segments(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    let mut output: Vec<&str> = Vec::new();
    for x in url.split('/') {
        if !x.is_empty() && x != "." {
            if x == ".." {
                output.pop();
            } else {
                output.push(x);
            }
        }
    }
    let first = if url.starts_with('/') { "/" } else { "" };
    let last = if url.ends_with('/') && !output.is_empty() {
        "/"
    } else {
        ""
    };
    format!("{first}{}{last}", output.join("/"))
}

const IPV4_PAT: &str = r"(?:[0-9]{1,3}\.){3}[0-9]{1,3}";

/// botocore `utils.IPV4_RE` (`^...$`, where Python's `$` also matches
/// before a trailing newline).
pub fn is_ipv4(s: &str) -> bool {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| (1..=3).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
}

/// botocore `utils.IPV6_ADDRZ_RE`.
fn ipv6_addrz_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let hex = "[0-9A-Fa-f]{1,4}";
        let ls32 = format!("(?:{hex}:{hex}|{IPV4_PAT})");
        let variations = [
            format!("(?:{hex}:){{6}}{ls32}"),
            format!("::(?:{hex}:){{5}}{ls32}"),
            format!("(?:{hex})?::(?:{hex}:){{4}}{ls32}"),
            format!("(?:(?:{hex}:)?{hex})?::(?:{hex}:){{3}}{ls32}"),
            format!("(?:(?:{hex}:){{0,2}}{hex})?::(?:{hex}:){{2}}{ls32}"),
            format!("(?:(?:{hex}:){{0,3}}{hex})?::{hex}:{ls32}"),
            format!("(?:(?:{hex}:){{0,4}}{hex})?::{ls32}"),
            format!("(?:(?:{hex}:){{0,5}}{hex})?::{hex}"),
            format!("(?:(?:{hex}:){{0,6}}{hex})?::"),
        ];
        let ipv6 = format!("(?:{})", variations.join("|"));
        let unreserved = r"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._!\-~";
        let zone = format!("(?:%25|%)(?:[{unreserved}]|%[a-fA-F0-9]{{2}})+");
        Regex::new(&format!(r"\A\[{ipv6}(?:{zone})?\]\n?\z")).unwrap()
    })
}

/// botocore `utils.is_valid_ipv4_endpoint_url`. `Err` is the `TypeError`
/// Python raises when the URL has no hostname.
pub fn is_valid_ipv4_endpoint_url(url: &str) -> Result<bool, String> {
    let parsed = urlparse(url).map_err(|e| e.0)?;
    match parsed.hostname() {
        Some(h) => Ok(is_ipv4(&h)),
        None => Err("expected string or bytes-like object, got 'NoneType'".into()),
    }
}

/// botocore `utils.is_valid_ipv6_endpoint_url`.
pub fn is_valid_ipv6_endpoint_url(url: &str) -> Result<bool, String> {
    if url.contains(['\t', '\r', '\n']) {
        return Ok(false);
    }
    let parsed = urlparse(url).map_err(|e| e.0)?;
    let host = match parsed.hostname() {
        Some(h) => h,
        None => "None".to_string(),
    };
    Ok(ipv6_addrz_re().is_match(&format!("[{host}]")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlparse_matches_python() {
        let p = urlparse("https://Example.com:8443/a/b;x?q=1#f").unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(p.netloc, "Example.com:8443");
        assert_eq!(p.path, "/a/b");
        assert_eq!(p.params, "x");
        assert_eq!(p.query, "q=1");
        assert_eq!(p.fragment, "f");
        assert_eq!(p.hostname().as_deref(), Some("example.com"));
        assert_eq!(p.port(), Ok(Some(8443)));
        assert!(urlparse("http://[::1").is_err());
        assert!(urlparse("http://[1.2.3.4]/").is_err());
        assert!(urlparse("http://h:99999").unwrap().port().is_err());
        assert!(urlparse("http://h:abc").unwrap().port().is_err());
    }

    #[test]
    fn dot_segments() {
        assert_eq!(normalize_url_path(""), "/");
        assert_eq!(normalize_url_path("/a/./b/../c//d/"), "/a/c/d/");
        assert_eq!(normalize_url_path("/.."), "/");
        assert_eq!(quote("/a b/é", "/"), "/a%20b/%C3%A9");
    }

    #[test]
    fn ip_detection() {
        assert_eq!(is_valid_ipv4_endpoint_url("http://1.2.3.4:80"), Ok(true));
        assert_eq!(
            is_valid_ipv6_endpoint_url("http://[fe80::1%25en0]"),
            Ok(true)
        );
        assert_eq!(is_valid_ipv6_endpoint_url("http://example.com"), Ok(false));
    }
}
