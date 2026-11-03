//! Building the request URL the way botocore does
//! (`botocore/awsrequest.py` `prepare_request_dict`, `_urljoin`;
//! `botocore/utils.py` `percent_encode_sequence`, `percent_encode`;
//! `urllib.parse.quote`, `urlsplit`, `urlunsplit`).

/// `urllib.parse.quote(s, safe)`: ASCII letters, digits, `_.-~`, and the
/// characters of `safe` are kept; every other UTF-8 byte becomes `%XX`.
pub fn quote(s: &str, safe: &str) -> String {
    quote_bytes(s.as_bytes(), safe)
}

pub fn quote_bytes(bytes: &[u8], safe: &str) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        let c = b as char;
        if b.is_ascii_alphanumeric()
            || matches!(c, '_' | '.' | '-' | '~')
            || (b.is_ascii() && safe.contains(c))
        {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `utils.percent_encode` with `SAFE_CHARS` (`-._~`).
pub fn percent_encode(s: &str) -> String {
    quote(s, "-._~")
}

/// `utils.percent_encode_sequence` for a list of pairs (list values are
/// already expanded into repeated pairs).
pub fn percent_encode_sequence(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `urllib.parse.urlsplit` result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SplitUrl {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
}

/// `urllib.parse.urlsplit` (without the bracket validation).
pub fn urlsplit(url: &str) -> SplitUrl {
    let mut rest = url;
    let mut out = SplitUrl::default();
    if let Some(i) = rest.find(':') {
        let scheme = &rest[..i];
        if !scheme.is_empty()
            && scheme.as_bytes()[0].is_ascii_alphabetic()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            out.scheme = scheme.to_ascii_lowercase();
            rest = &rest[i + 1..];
        }
    }
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        out.netloc = after[..end].to_string();
        rest = &after[end..];
    }
    if let Some(i) = rest.find('#') {
        out.fragment = rest[i + 1..].to_string();
        rest = &rest[..i];
    }
    if let Some(i) = rest.find('?') {
        out.query = rest[i + 1..].to_string();
        rest = &rest[..i];
    }
    out.path = rest.to_string();
    out
}

/// `urllib.parse.urlunsplit`.
pub fn urlunsplit(p: &SplitUrl) -> String {
    let mut url = p.path.clone();
    if !p.netloc.is_empty() || (!p.scheme.is_empty() && !url.starts_with("//")) {
        if !url.is_empty() && !url.starts_with('/') {
            url = format!("/{url}");
        }
        url = format!("//{}{url}", p.netloc);
    }
    if !p.scheme.is_empty() {
        url = format!("{}:{url}", p.scheme);
    }
    if !p.query.is_empty() {
        url = format!("{url}?{}", p.query);
    }
    if !p.fragment.is_empty() {
        url = format!("{url}#{}", p.fragment);
    }
    url
}

/// `awsrequest._urljoin`.
pub fn urljoin(endpoint_url: &str, url_path: &str, host_prefix: Option<&str>) -> String {
    let mut p = urlsplit(endpoint_url);
    let new_path = if url_path.is_empty() || url_path == "/" {
        if p.path.is_empty() {
            "/".to_string()
        } else {
            p.path.clone()
        }
    } else if p.path.ends_with('/') && url_path.starts_with('/') {
        format!("{}{url_path}", &p.path[..p.path.len() - 1])
    } else {
        format!("{}{url_path}", p.path)
    };
    if let Some(prefix) = host_prefix {
        p.netloc = format!("{prefix}{}", p.netloc);
    }
    p.path = new_path;
    urlunsplit(&p)
}

/// The URL `prepare_request_dict` builds: the endpoint joined with the
/// serialized path (and host prefix), then the percent-encoded query.
pub fn request_url(
    endpoint_url: &str,
    url_path: &str,
    query: &[(String, String)],
    host_prefix: Option<&str>,
) -> String {
    let mut url = urljoin(endpoint_url, url_path, host_prefix);
    if !query.is_empty() {
        let encoded = percent_encode_sequence(query);
        if url.contains('?') {
            url.push('&');
        } else {
            url.push('?');
        }
        url.push_str(&encoded);
    }
    url
}

/// The parts of a URL the HTTP layer and the signer need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    pub scheme: String,
    /// `urlsplit(url).hostname`: lowercased, without IPv6 brackets.
    pub hostname: String,
    pub port: Option<u16>,
    /// The request target (`path?query`, path defaulting to `/`).
    pub target: String,
    pub path: String,
    pub query: String,
}

impl UrlParts {
    pub fn parse(url: &str) -> Option<UrlParts> {
        let s = urlsplit(url);
        let netloc = s
            .netloc
            .rsplit_once('@')
            .map_or(s.netloc.as_str(), |(_, h)| h);
        let (host, port) = if let Some(rest) = netloc.strip_prefix('[') {
            let close = rest.find(']')?;
            let port = rest[close + 1..].strip_prefix(':');
            (&rest[..close], port)
        } else {
            match netloc.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (netloc, None),
            }
        };
        let port = match port {
            None | Some("") => None,
            Some(p) => Some(p.parse::<u16>().ok()?),
        };
        let path = if s.path.is_empty() {
            "/".to_string()
        } else {
            s.path.clone()
        };
        let target = if s.query.is_empty() {
            path.clone()
        } else {
            format!("{path}?{}", s.query)
        };
        Some(UrlParts {
            scheme: s.scheme,
            hostname: host.to_lowercase(),
            port,
            target,
            path,
            query: s.query,
        })
    }

    pub fn default_port(&self) -> u16 {
        if self.scheme == "https" { 443 } else { 80 }
    }

    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.default_port())
    }

    /// `auth._host_from_url`: the host with IPv6 brackets and a
    /// non-default port.
    pub fn host_header(&self) -> String {
        let host = if self.hostname.contains(':') {
            format!("[{}]", self.hostname)
        } else {
            self.hostname.clone()
        };
        match self.port {
            Some(p) if p != self.default_port() => format!("{host}:{p}"),
            _ => host,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Expected values from `botocore.awsrequest.prepare_request_dict`.
    #[test]
    fn prepare_request_dict_urls() {
        assert_eq!(
            request_url("https://sts.amazonaws.com", "/", &[], None),
            "https://sts.amazonaws.com/"
        );
        assert_eq!(
            request_url("http://127.0.0.1:18081/base/", "/", &[], None),
            "http://127.0.0.1:18081/base/"
        );
        assert_eq!(
            request_url("http://h/base/", "/x/y", &[], None),
            "http://h/base/x/y"
        );
        assert_eq!(
            request_url("http://h/base", "/x", &[], None),
            "http://h/base/x"
        );
        assert_eq!(
            request_url(
                "http://127.0.0.1:18081",
                "/bkt",
                &q(&[
                    ("list-type", "2"),
                    ("prefix", "a b+c/~*"),
                    ("encoding-type", "url")
                ]),
                None
            ),
            "http://127.0.0.1:18081/bkt?list-type=2&prefix=a%20b%2Bc%2F~%2A&encoding-type=url"
        );
        assert_eq!(
            request_url(
                "https://s3.amazonaws.com",
                "/b?uploads",
                &q(&[("k", "é")]),
                None
            ),
            "https://s3.amazonaws.com/b?uploads&k=%C3%A9"
        );
        assert_eq!(
            request_url("https://example.com", "/", &[], Some("data.")),
            "https://data.example.com/"
        );
    }

    #[test]
    fn host_header() {
        let p = UrlParts::parse("https://Example.com:443/a?b=c").unwrap();
        assert_eq!(p.host_header(), "example.com");
        assert_eq!(p.target, "/a?b=c");
        let p = UrlParts::parse("http://[::1]:8080").unwrap();
        assert_eq!(p.host_header(), "[::1]:8080");
        assert_eq!(p.target, "/");
    }
}
