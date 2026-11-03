//! `cloudfront sign` / `sign-cookies`
//! (`awscli/customizations/cloudfront/sign.py`, `botocore/signers.py`
//! `CloudFrontSigner`).

use base64::Engine as _;
use regex::Regex;

use crate::cli::Fail;
use crate::commands::services::basic::{self, ArgDef, Args};
use crate::driver::{Context, display_response};
use crate::error::{CliError, ExitCode};
use crate::output::Response;
use crate::value::{Map, Value};

const DATE_FORMAT: &str = "Supported formats include:
        YYYY-MM-DD (which means 0AM UTC of that day),
        YYYY-MM-DDThh:mm:ss (with default timezone as UTC),
        YYYY-MM-DDThh:mm:ss+hh:mm or YYYY-MM-DDThh:mm:ss-hh:mm (with offset),
        or EpochTime (which always means UTC).
        Do NOT use YYYYMMDD, because it will be treated as EpochTime.";

const SIGNING_QUERY_PARAMS: &[&str] = &[
    "Expires",
    "Policy",
    "Signature",
    "Key-Pair-Id",
    "Hash-Algorithm",
];

fn signing_args() -> Vec<ArgDef> {
    vec![
        ArgDef::new("key-pair-id").required(),
        ArgDef::new("private-key").required().type_name("blob"),
        ArgDef::new("date-less-than").required(),
        ArgDef::new("date-greater-than"),
        ArgDef::new("ip-address"),
        ArgDef::new("hash-algorithm").choices(&["SHA1", "SHA256"]),
    ]
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Sign,
    Cookies,
}

pub(super) fn sign(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut defs = vec![
        ArgDef::new("url").required(),
        ArgDef::new("policy-resource"),
    ];
    defs.extend(signing_args());
    let Some(a) = basic::parse(ctx, &["cloudfront", "sign"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    run_main(ctx, Kind::Sign, &a)
}

pub(super) fn sign_cookies(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut defs = vec![ArgDef::new("resource").required()];
    defs.extend(signing_args());
    let Some(a) = basic::parse(ctx, &["cloudfront", "sign-cookies"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    run_main(ctx, Kind::Cookies, &a)
}

fn render(ctx: &Context, f: Fail) -> CliError {
    crate::cli::render_fail(f, &ctx.globals, &ctx.session)
}

/// `BaseSignCommand._run_main`.
fn run_main(ctx: &Context, kind: Kind, a: &Args) -> Result<ExitCode, CliError> {
    let fail = |f: Fail| render(ctx, f);
    let url = a.str("url").unwrap_or_default();
    let resource = match kind {
        Kind::Sign => policy_resource_for_url(&url, a.str("policy-resource").as_deref()),
        Kind::Cookies => {
            let r = a.str("resource").unwrap_or_default();
            validate_resource(&r, "resource", true).map(|_| r)
        }
    }
    .map_err(fail)?;
    let less = parse_date(
        kind,
        &a.str("date-less-than").unwrap_or_default(),
        "date-less-than",
    )
    .map_err(fail)?;
    let greater = match a.str("date-greater-than") {
        Some(g) => {
            let g = parse_date(kind, &g, "date-greater-than").map_err(fail)?;
            if g >= less {
                return Err(fail(Fail::ParamValidation(
                    "Invalid date range: --date-greater-than (the active date) must be before --date-less-than (the expiration date).".to_string(),
                )));
            }
            Some(g)
        }
        None => None,
    };
    let ip = a.str("ip-address");
    if let Some(ip) = &ip {
        validate_ip(ip).map_err(fail)?;
    }
    let key = match a.get("private-key") {
        Some(Value::Bytes(b)) => match String::from_utf8(b.clone()) {
            Ok(s) => s,
            Err(_) => {
                return Err(fail(Fail::Python(
                    "Unsupported private key. The private key must be an RSA or ECDSA (P-256) private key in PEM format.".to_string(),
                )));
            }
        },
        Some(v) => super::super::cloudwatch::value_str(v),
        None => String::new(),
    };
    let signer = build_signer(&key, a.str("hash-algorithm").as_deref()).map_err(fail)?;
    let custom = greater.is_some()
        || ip.is_some()
        || resource.contains('*')
        || (kind == Kind::Sign && resource != url);
    let key_pair_id = a.str("key-pair-id").unwrap_or_default();
    let mut params: Vec<(String, String)> = Vec::new();
    let policy = if custom {
        let p = build_policy(&resource, less, greater, ip.as_deref());
        params.push(("Policy".to_string(), url_b64encode(p.as_bytes())));
        p
    } else {
        params.push(("Expires".to_string(), less.to_string()));
        build_policy(&resource, less, None, None)
    };
    let signature = signer.sign(policy.as_bytes()).map_err(fail)?;
    params.push(("Signature".to_string(), url_b64encode(&signature)));
    params.push(("Key-Pair-Id".to_string(), key_pair_id));
    if signer.hash() != "SHA1" {
        params.push(("Hash-Algorithm".to_string(), signer.hash().to_string()));
    }
    match kind {
        Kind::Sign => {
            let sep = if url.contains('?') { '&' } else { '?' };
            let query: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
            basic::print(&format!("{url}{sep}{}", query.join("&")));
        }
        Kind::Cookies => {
            let mut m = Map::new();
            for (k, v) in params {
                m.insert(format!("CloudFront-{k}"), Value::Str(v));
            }
            display_response(ctx, "sign-cookies", Response::Single(Value::Map(m)))?;
        }
    }
    Ok(ExitCode::Success)
}

/// `_parse_date`: `int(datetime2timestamp(parse_to_aware_datetime(value)))`.
fn parse_date(kind: Kind, value: &str, arg: &str) -> Result<i64, Fail> {
    crate::protocol::timestamp::utc_epoch_seconds(&Value::str(value)).map_err(|e| match kind {
        Kind::Sign => Fail::Python(format!("Invalid value for --{arg}: {e}. {DATE_FORMAT}")),
        Kind::Cookies => Fail::ParamValidation(format!(
            "Invalid value for --{arg}: \"{value}\" is not a valid date. {DATE_FORMAT}"
        )),
    })
}

/// `SignCommand._get_policy_resource`.
fn policy_resource_for_url(url: &str, policy_resource: Option<&str>) -> Result<String, Fail> {
    validate_resource(url, "url", false)?;
    let query = url
        .split_once('?')
        .map(|(_, q)| q.split_once('#').map_or(q, |(q, _)| q))
        .unwrap_or("");
    let mut found: Vec<&str> = Vec::new();
    for part in query.split('&').filter(|p| !p.is_empty()) {
        let name = part.split_once('=').map_or(part, |(n, _)| n);
        let name = percent_encoding::percent_decode_str(&name.replace('+', " "))
            .decode_utf8_lossy()
            .into_owned();
        if let Some(p) = SIGNING_QUERY_PARAMS.iter().find(|p| **p == name)
            && !found.contains(p)
        {
            found.push(p);
        }
    }
    if !found.is_empty() {
        found.sort();
        return Err(Fail::ParamValidation(format!(
            "Invalid value for --url: the URL already contains the CloudFront signing parameters {}. Remove them before signing the URL.",
            found.join(", ")
        )));
    }
    let Some(resource) = policy_resource else {
        return Ok(url.to_string());
    };
    validate_resource(resource, "policy-resource", true)?;
    let mut pattern = String::from("(?s)^(?:");
    for c in resource.chars() {
        match c {
            '*' => pattern.push_str(".*"),
            '?' => pattern.push('.'),
            c => pattern.push_str(&regex::escape(&c.to_string())),
        }
    }
    pattern.push_str(")$");
    let matches = Regex::new(&pattern).is_ok_and(|re| re.is_match(url));
    if !matches {
        return Err(Fail::ParamValidation(format!(
            "Invalid value for --url: \"{url}\" does not match --policy-resource \"{resource}\", so CloudFront would reject the signature."
        )));
    }
    Ok(resource.to_string())
}

/// `_validate_resource`.
fn validate_resource(resource: &str, arg: &str, allow_wildcards: bool) -> Result<(), Fail> {
    let error = if resource.is_empty() {
        "the resource must not be empty.".to_string()
    } else if resource
        .chars()
        .any(|c| c == '"' || c == '\\' || (c as u32) < 0x20 || c as u32 == 0x7f)
    {
        "the resource must not contain double quotes, backslashes, or control characters."
            .to_string()
    } else if !resource.is_ascii() || resource.contains(' ') {
        "the resource must only contain ASCII characters without spaces. Percent-encode other characters in the path and use the punycode (xn--) form of internationalized domain names.".to_string()
    } else if resource.contains('#') {
        "the resource must not contain a URL fragment (\"#\"), which is never sent to CloudFront."
            .to_string()
    } else if (allow_wildcards && resource.contains('*')) || is_http_url(resource) {
        return Ok(());
    } else {
        format!(
            "\"{resource}\" is not a valid URL. The URL must be an absolute http:// or https:// URL, for example https://d111111abcdef8.cloudfront.net/image.jpg"
        )
    };
    Err(Fail::ParamValidation(format!(
        "Invalid value for --{arg}: {error}"
    )))
}

/// `_is_http_url` (`urlsplit(url)`, `parsed.port`, `parsed.hostname`).
fn is_http_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let Some(rest) = rest.strip_prefix("//") else {
        return false;
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let (host, port) = if let Some(stripped) = host_port.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, p)) => (h, p.strip_prefix(':')),
            None => return false,
        }
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        }
    };
    if let Some(p) = port
        && !p.is_empty()
    {
        match p.parse::<u32>() {
            Ok(n) if n <= 65535 && p.chars().all(|c| c.is_ascii_digit()) => {}
            _ => return false,
        }
    }
    !host.is_empty()
}

/// `_validate_signing_args` (`_reject_ipv6_address`, the IPv4 checks).
fn validate_ip(ip: &str) -> Result<(), Fail> {
    let (addr, prefix) = ip.split_once('/').map_or((ip, None), |(a, p)| (a, Some(p)));
    let is_ipv6 = addr.parse::<std::net::Ipv6Addr>().is_ok()
        && prefix.is_none_or(|p| p.parse::<u8>().is_ok_and(|n| n <= 128));
    if is_ipv6 {
        return Err(Fail::ParamValidation(format!(
            "Invalid value for --ip-address: \"{ip}\" is an IPv6 address. CloudFront only supports IPv4 addresses and ranges."
        )));
    }
    let format = Regex::new(r"^\d{1,3}(\.\d{1,3}){3}(/\d{1,2})?$").expect("regex");
    let valid = format.is_match(ip)
        && addr
            .split('.')
            .all(|o| o.parse::<u16>().is_ok_and(|n| n <= 255))
        && prefix.is_none_or(|p| p.parse::<u8>().is_ok_and(|n| n <= 32));
    if !valid {
        return Err(Fail::ParamValidation(format!(
            "Invalid value for --ip-address: \"{ip}\" is not a valid IPv4 address or CIDR range. Format: x.x.x.x/x or x.x.x.x"
        )));
    }
    Ok(())
}

/// `CloudFrontSigner.build_policy`.
fn build_policy(resource: &str, less: i64, greater: Option<i64>, ip: Option<&str>) -> String {
    let mut condition = format!("{{\"DateLessThan\":{{\"AWS:EpochTime\":{less}}}");
    if let Some(ip) = ip.filter(|i| !i.is_empty()) {
        let ip = if ip.contains('/') {
            ip.to_string()
        } else {
            format!("{ip}/32")
        };
        condition.push_str(&format!(",\"IpAddress\":{{\"AWS:SourceIp\":\"{ip}\"}}"));
    }
    if let Some(g) = greater {
        condition.push_str(&format!(",\"DateGreaterThan\":{{\"AWS:EpochTime\":{g}}}"));
    }
    condition.push('}');
    let resource = crate::pyfmt::json_dumps_default(&Value::str(resource));
    format!("{{\"Statement\":[{{\"Resource\":{resource},\"Condition\":{condition}}}]}}")
}

/// `CloudFrontSigner._url_b64encode`.
fn url_b64encode(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD
        .encode(data)
        .replace('+', "-")
        .replace('=', "_")
        .replace('/', "~")
}

enum Signer {
    Rsa(Box<rsa::RsaPrivateKey>, &'static str),
    Ecdsa(Box<p256::ecdsa::SigningKey>),
}

impl Signer {
    fn hash(&self) -> &'static str {
        match self {
            Signer::Rsa(_, h) => h,
            Signer::Ecdsa(_) => "SHA256",
        }
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Fail> {
        use sha1::Digest as _;
        let failed = |e: String| {
            Fail::Python(format!(
                "Failed to sign the CloudFront policy with the private key: {e}"
            ))
        };
        match self {
            Signer::Rsa(key, "SHA1") => key
                .sign(
                    rsa::Pkcs1v15Sign::new::<sha1::Sha1>(),
                    &sha1::Sha1::digest(message),
                )
                .map_err(|e| failed(e.to_string())),
            Signer::Rsa(key, _) => key
                .sign(
                    rsa::Pkcs1v15Sign::new::<sha2::Sha256>(),
                    &sha2::Sha256::digest(message),
                )
                .map_err(|e| failed(e.to_string())),
            Signer::Ecdsa(key) => {
                use p256::ecdsa::signature::hazmat::PrehashSigner as _;
                let sig: p256::ecdsa::Signature = key
                    .sign_prehash(&sha2::Sha256::digest(message))
                    .map_err(|e| failed(e.to_string()))?;
                Ok(sig.to_der().as_bytes().to_vec())
            }
        }
    }
}

/// `_pem_block(pem, *labels)`.
fn pem_block(pem: &str, labels: &[&str]) -> Option<String> {
    for label in labels {
        let begin = format!("-----BEGIN {label}-----");
        let end = format!("-----END {label}-----");
        if let Some(s) = pem.find(&begin)
            && let Some(e) = pem[s..].find(&end)
        {
            return Some(pem[s..s + e + end.len()].to_string());
        }
    }
    None
}

fn pem_der(block: &str) -> Option<Vec<u8>> {
    let body: String = block
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .flat_map(|l| l.split_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

fn rsa_from_pem(pem: &str) -> Option<rsa::RsaPrivateKey> {
    use rsa::pkcs1::DecodeRsaPrivateKey as _;
    use rsa::pkcs8::DecodePrivateKey as _;
    let block = pem_block(pem, &["RSA PRIVATE KEY", "PRIVATE KEY"])?;
    let der = pem_der(&block)?;
    if block.contains("BEGIN RSA PRIVATE KEY") {
        rsa::RsaPrivateKey::from_pkcs1_der(&der).ok()
    } else {
        rsa::RsaPrivateKey::from_pkcs8_der(&der).ok()
    }
}

fn ec_from_pem(pem: &str) -> Option<p256::ecdsa::SigningKey> {
    use p256::pkcs8::DecodePrivateKey as _;
    let block = pem_block(pem, &["EC PRIVATE KEY", "PRIVATE KEY"])?;
    let der = pem_der(&block)?;
    if block.contains("BEGIN EC PRIVATE KEY") {
        p256::SecretKey::from_sec1_der(&der)
            .ok()
            .map(p256::ecdsa::SigningKey::from)
    } else {
        p256::ecdsa::SigningKey::from_pkcs8_der(&der).ok()
    }
}

/// `_build_signer`.
fn build_signer(pem: &str, hash: Option<&str>) -> Result<Signer, Fail> {
    if pem.contains("BEGIN ENCRYPTED PRIVATE KEY") || pem.contains("Proc-Type: 4,ENCRYPTED") {
        return Err(Fail::Python(
            "Encrypted private keys are not supported. Provide an unencrypted RSA or ECDSA (P-256) private key in PEM format.".to_string(),
        ));
    }
    let (try_rsa, try_ec) = if pem.contains("BEGIN RSA PRIVATE KEY") {
        (true, false)
    } else if pem.contains("BEGIN EC PRIVATE KEY") {
        (false, true)
    } else if pem.contains("BEGIN PRIVATE KEY") {
        (true, true)
    } else {
        (false, false)
    };
    if try_rsa && let Some(key) = rsa_from_pem(pem) {
        let hash = if hash == Some("SHA256") {
            "SHA256"
        } else {
            "SHA1"
        };
        return Ok(Signer::Rsa(Box::new(key), hash));
    }
    if try_ec && let Some(key) = ec_from_pem(pem) {
        if !matches!(hash, None | Some("SHA256")) {
            return Err(Fail::ParamValidation(format!(
                "The {} hash algorithm cannot be used with ECDSA keys, which only support SHA256. Remove --hash-algorithm or set it to SHA256.",
                hash.unwrap_or_default()
            )));
        }
        return Ok(Signer::Ecdsa(Box::new(key)));
    }
    Err(Fail::Python(
        "Unsupported key type or invalid private key. CloudFront signing supports RSA and ECDSA (P-256) private keys in PEM format (PKCS#1, PKCS#8, or SEC1).".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canned_policy() {
        assert_eq!(
            build_policy("https://d/a", 10, None, None),
            r#"{"Statement":[{"Resource":"https://d/a","Condition":{"DateLessThan":{"AWS:EpochTime":10}}}]}"#
        );
        assert_eq!(url_b64encode(&[0xfb, 0xff]), "-~8_");
        assert!(is_http_url("https://d1.cloudfront.net/a.jpg"));
        assert!(!is_http_url("ftp://x"));
        assert!(!is_http_url("https://h:99999/"));
    }
}
