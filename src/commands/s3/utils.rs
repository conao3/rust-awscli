//! Helpers of `awscli/customizations/s3/utils.py`: S3 path parsing, size
//! formatting, local paths, content types, timestamps, and the CLI to
//! request parameter mapping.

use std::sync::OnceLock;

use regex::Regex;

use crate::cli::Fail;
use crate::value::{Map, Value};

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("regex"))
}

fn accesspoint_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(
        &RE,
        r"^(?P<bucket>arn:(aws).*:s3:[a-z\-0-9]*:[0-9]{12}:accesspoint[:/][^/]+)/?(?P<key>.*)$",
    )
}

fn outpost_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(
        &RE,
        r"^(?P<bucket>arn:(aws).*:s3-outposts:[a-z\-0-9]+:[0-9]{12}:outpost[/:][a-zA-Z0-9\-]{1,63}[/:]accesspoint[/:][a-zA-Z0-9\-]{1,63})[/:]?(?P<key>.*)$",
    )
}

fn outpost_bucket_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(
        &RE,
        r"^(?P<bucket>arn:(aws).*:s3-outposts:[a-z\-0-9]+:[0-9]{12}:outpost[/:][a-zA-Z0-9\-]{1,63}[/:]bucket[/:][a-zA-Z0-9\-]{1,63})[/:]?(?P<key>.*)$",
    )
}

fn object_lambda_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(
        &RE,
        r"^(?P<bucket>arn:(aws).*:s3-object-lambda:[a-z\-0-9]+:[0-9]{12}:accesspoint[/:][a-zA-Z0-9\-]{1,63})[/:]?(?P<key>.*)$",
    )
}

/// `block_unsupported_resources`.
pub(crate) fn block_unsupported_resources(s3_path: &str) -> Result<(), Fail> {
    if object_lambda_re().is_match(s3_path) {
        return Err(Fail::ParamValidation(
            "s3 commands do not support S3 Object Lambda resources. Use s3api commands instead."
                .into(),
        ));
    }
    if outpost_bucket_re().is_match(s3_path) {
        return Err(Fail::ParamValidation(
            "s3 commands do not support Outpost Bucket ARNs. Use s3control commands instead."
                .into(),
        ));
    }
    Ok(())
}

/// `find_bucket_key`.
pub(crate) fn find_bucket_key(s3_path: &str) -> Result<(String, String), Fail> {
    block_unsupported_resources(s3_path)?;
    for r in [accesspoint_re(), outpost_re()] {
        if let Some(c) = r.captures(s3_path) {
            return Ok((c["bucket"].to_string(), c["key"].to_string()));
        }
    }
    Ok(match s3_path.split_once('/') {
        Some((b, k)) => (b.to_string(), k.to_string()),
        None => (s3_path.to_string(), String::new()),
    })
}

/// `split_s3_bucket_key`.
pub(crate) fn split_s3_bucket_key(s3_path: &str) -> Result<(String, String), Fail> {
    find_bucket_key(s3_path.strip_prefix("s3://").unwrap_or(s3_path))
}

/// `botocore.utils.is_s3express_bucket`.
pub(crate) fn is_s3express_bucket(bucket: &str) -> bool {
    bucket.ends_with("--x-s3")
}

/// `human_readable_size`.
pub(crate) fn human_readable_size(value: f64) -> String {
    const SUFFIXES: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let base = 1024f64;
    if value == 1.0 {
        return "1 Byte".to_string();
    }
    if value < base {
        return format!("{} Bytes", value.trunc() as i64);
    }
    for (i, suffix) in SUFFIXES.iter().enumerate() {
        let unit = base.powi(i as i32 + 2);
        if (value / unit * base).round_ties_even() < base {
            return format!("{:.1} {suffix}", base * value / unit);
        }
    }
    // Python falls off the loop and returns None.
    "None".to_string()
}

/// `human_readable_to_int`.
pub(crate) fn human_readable_to_int(value: &str) -> Result<i64, String> {
    let lower = value.to_lowercase();
    let suffix = if lower.ends_with("ib") {
        lower.get(lower.len().saturating_sub(3)..).unwrap_or("")
    } else {
        lower.get(lower.len().saturating_sub(2)..).unwrap_or("")
    };
    let mult: Option<i64> = match suffix {
        "kb" | "kib" => Some(1024),
        "mb" | "mib" => Some(1024i64.pow(2)),
        "gb" | "gib" => Some(1024i64.pow(3)),
        "tb" | "tib" => Some(1024i64.pow(4)),
        _ => None,
    };
    let int = |s: &str| -> Option<i64> {
        crate::cli::pynum::py_int(s).and_then(|b| i64::try_from(b).ok())
    };
    match mult {
        Some(m) if lower.chars().count() >= 2 => {
            let num = &lower[..lower.len() - suffix.len()];
            int(num).map(|n| n * m).ok_or_else(|| {
                format!(
                    "invalid literal for int() with base 10: {}",
                    crate::pyfmt::str_repr(num)
                )
            })
        }
        _ => int(&lower).ok_or_else(|| format!("Invalid size value: {value}")),
    }
}

/// `os.path.abspath`.
pub(crate) fn abspath(path: &str) -> String {
    let joined = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        format!("{cwd}/{path}")
    };
    normpath(&joined)
}

/// `os.path.normpath` (POSIX).
pub(crate) fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut comps: Vec<&str> = vec![];
    for c in path.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c != ".." || (initial == 0 && comps.is_empty()) || comps.last() == Some(&"..") {
            comps.push(c);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let joined = comps.join("/");
    let out = format!("{}{joined}", "/".repeat(initial));
    if out.is_empty() { ".".to_string() } else { out }
}

/// `os.path.dirname`.
pub(crate) fn dirname(path: &str) -> String {
    let i = path.rfind('/').map_or(0, |i| i + 1);
    let head = &path[..i];
    if !head.is_empty() && head.chars().any(|c| c != '/') {
        head.trim_end_matches('/').to_string()
    } else {
        head.to_string()
    }
}

/// `os.path.basename`.
pub(crate) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `os.path.join` of two components.
pub(crate) fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `os.path.relpath(path, start)` (both absolute after `abspath`).
pub(crate) fn relpath(path: &str, start: &str) -> String {
    let p = abspath(path);
    let s = abspath(start);
    let pl: Vec<&str> = p.split('/').filter(|x| !x.is_empty()).collect();
    let sl: Vec<&str> = s.split('/').filter(|x| !x.is_empty()).collect();
    let common = pl.iter().zip(&sl).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = vec![".."; sl.len() - common];
    parts.extend(&pl[common..]);
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// `relative_path(filename)` relative to the current directory.
pub(crate) fn relative_path(filename: &str) -> String {
    let i = filename.rfind('/').map_or(0, |i| i + 1);
    let (dir, base) = (&filename[..i], &filename[i..]);
    let dir = if dir.len() > 1 {
        dir.trim_end_matches('/')
    } else {
        dir
    };
    let rel = relpath(if dir.is_empty() { "." } else { dir }, ".");
    join(&rel, base)
}

include!("mimetypes_table.rs");

fn system_types() -> &'static Vec<(String, String)> {
    static TYPES: OnceLock<Vec<(String, String)>> = OnceLock::new();
    TYPES.get_or_init(|| {
        const KNOWN: &[&str] = &[
            "/etc/mime.types",
            "/etc/httpd/mime.types",
            "/etc/httpd/conf/mime.types",
            "/etc/apache/mime.types",
            "/etc/apache2/mime.types",
            "/usr/local/etc/httpd/conf/mime.types",
            "/usr/local/lib/netscape/mime.types",
            "/usr/local/etc/httpd/conf/mime.types",
            "/usr/local/etc/mime.types",
        ];
        let mut out = vec![];
        for f in KNOWN {
            let Ok(text) = std::fs::read_to_string(f) else {
                continue;
            };
            for line in text.lines() {
                let mut words: Vec<&str> = line.split_whitespace().collect();
                if let Some(i) = words.iter().position(|w| w.starts_with('#')) {
                    words.truncate(i);
                }
                let Some((ty, suffixes)) = words.split_first() else {
                    continue;
                };
                for s in suffixes {
                    out.push((format!(".{s}"), ty.to_string()));
                }
            }
        }
        out
    })
}

fn types_map_get(ext: &str) -> Option<String> {
    if let Some((_, t)) = system_types().iter().rev().find(|(e, _)| e == ext) {
        return Some(t.clone());
    }
    TYPES_MAP
        .binary_search_by(|(e, _)| e.cmp(&ext))
        .ok()
        .map(|i| TYPES_MAP[i].1.to_string())
}

/// `os.path.splitext`.
fn splitext(path: &str) -> (&str, &str) {
    let sep = path.rfind('/').map_or(0, |i| i + 1);
    let name = &path[sep..];
    let lead = name.len() - name.trim_start_matches('.').len();
    match name[lead..].rfind('.') {
        Some(i) => path.split_at(sep + lead + i),
        None => (path, ""),
    }
}

/// `guess_content_type` (`mimetypes.guess_type(filename)[0]`, strict).
pub(crate) fn guess_content_type(filename: &str) -> Option<String> {
    const SUFFIX_MAP: &[(&str, &str)] = &[
        (".svgz", ".svg.gz"),
        (".tgz", ".tar.gz"),
        (".taz", ".tar.gz"),
        (".tz", ".tar.gz"),
        (".tbz2", ".tar.bz2"),
        (".txz", ".tar.xz"),
    ];
    const ENCODINGS: &[&str] = &[".gz", ".Z", ".bz2", ".xz", ".br"];
    let mut path = filename.to_string();
    let (b, e) = splitext(&path);
    let (mut base, mut ext) = (b.to_string(), e.to_string());
    while let Some((_, rep)) = SUFFIX_MAP.iter().find(|(s, _)| *s == ext.to_lowercase()) {
        path = format!("{base}{rep}");
        let (b, e) = splitext(&path);
        (base, ext) = (b.to_string(), e.to_string());
    }
    if ENCODINGS.contains(&ext.as_str()) {
        let (_, e) = splitext(&base);
        ext = e.to_string();
    }
    types_map_get(&ext.to_lowercase())
}

/// Seconds and microseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Instant {
    pub micros: i64,
}

impl Instant {
    pub(crate) fn secs(&self) -> i64 {
        self.micros.div_euclid(1_000_000)
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse a timestamp as the parsers produce it (ISO 8601 with an offset,
/// e.g. `2023-11-14T22:13:20+00:00` or `...20.123000Z`).
pub(crate) fn parse_timestamp(s: &str) -> Option<Instant> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let r = re(
        &RE,
        r"^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?(Z|[+-]\d{2}:?\d{2})?$",
    );
    let c = r.captures(s.trim())?;
    let n = |i: usize| c[i].parse::<i64>().unwrap_or(0);
    let days = days_from_civil(n(1), n(2), n(3));
    let mut secs = days * 86_400 + n(4) * 3600 + n(5) * 60 + n(6);
    if let Some(tz) = c.get(8).map(|m| m.as_str())
        && tz != "Z"
    {
        let sign = if tz.starts_with('-') { -1 } else { 1 };
        let digits: String = tz[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        let h: i64 = digits[..2].parse().unwrap_or(0);
        let m: i64 = digits[2..].parse().unwrap_or(0);
        secs -= sign * (h * 3600 + m * 60);
    }
    let frac = c.get(7).map_or(0, |f| {
        let mut d: String = f.as_str().chars().take(6).collect();
        while d.len() < 6 {
            d.push('0');
        }
        d.parse::<i64>().unwrap_or(0)
    });
    Some(Instant {
        micros: secs * 1_000_000 + frac,
    })
}

/// The local broken-down time of `secs` (`astimezone(tzlocal())`).
pub(crate) fn local_time(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe extern "C" {
        fn tzset();
    }
    unsafe {
        tzset();
        libc::localtime_r(&t, &mut tm);
    }
    (
        i64::from(tm.tm_year) + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
    )
}

/// `create_warning(path, error_message, skip_file)`: the message.
pub(crate) fn warning_message(path: &str, error_message: &str, skip_file: bool) -> String {
    let mut s = "warning: ".to_string();
    if skip_file {
        s.push_str(&format!("Skipping file {path}. "));
    }
    s.push_str(error_message);
    s
}

/// `RequestParamsMapper`.
pub(crate) mod mapper {
    use super::*;
    use crate::commands::s3::args::Args;

    fn set(p: &mut Map, k: &str, v: &Value) {
        p.insert(k.to_string(), v.clone());
    }

    fn general(p: &mut Map, cli: &Args) -> Result<(), Fail> {
        for (c, r) in [
            ("acl", "ACL"),
            ("storage_class", "StorageClass"),
            ("website_redirect", "WebsiteRedirectLocation"),
            ("content_type", "ContentType"),
            ("cache_control", "CacheControl"),
            ("content_disposition", "ContentDisposition"),
            ("content_encoding", "ContentEncoding"),
            ("content_language", "ContentLanguage"),
            ("expires", "Expires"),
        ] {
            if cli.truthy(c) {
                set(p, r, cli.get(c).unwrap());
            }
        }
        if cli.truthy("grants")
            && let Some(Value::List(grants)) = cli.get("grants")
        {
            for g in grants {
                let s = g.as_str().unwrap_or_default();
                let Some((perm, grantee)) = s.split_once('=') else {
                    return Err(Fail::ParamValidation(
                        "grants should be of the form permission=principal".into(),
                    ));
                };
                let param = match perm {
                    "read" => "GrantRead",
                    "full" => "GrantFullControl",
                    "readacl" => "GrantReadACP",
                    "writeacl" => "GrantWriteACP",
                    _ => {
                        return Err(Fail::ParamValidation(
                            "permission must be one of: read|readacl|writeacl|full".into(),
                        ));
                    }
                };
                p.insert(param.into(), Value::str(grantee));
            }
        }
        Ok(())
    }

    fn metadata(p: &mut Map, cli: &Args) {
        if cli.truthy("metadata") {
            set(p, "Metadata", cli.get("metadata").unwrap());
        }
    }

    fn tagging(p: &mut Map, cli: &Args) {
        if cli.truthy("tags") {
            let pairs: Vec<(String, String)> = crate::commands::s3::args::tag_list(cli)
                .iter()
                .filter_map(|t| {
                    let t = t.as_map()?;
                    Some((
                        t.get("Key")?.as_str()?.to_string(),
                        t.get("Value")?.as_str()?.to_string(),
                    ))
                })
                .collect();
            p.insert(
                "Tagging".into(),
                Value::str(crate::session::url::percent_encode_sequence(&pairs)),
            );
        }
    }

    fn sse(p: &mut Map, cli: &Args) {
        if cli.truthy("sse") {
            set(p, "ServerSideEncryption", cli.get("sse").unwrap());
        }
        if cli.truthy("sse_kms_key_id") {
            set(p, "SSEKMSKeyId", cli.get("sse_kms_key_id").unwrap());
        }
    }

    fn sse_c(p: &mut Map, cli: &Args) {
        if cli.truthy("sse_c") {
            set(p, "SSECustomerAlgorithm", cli.get("sse_c").unwrap());
            p.insert(
                "SSECustomerKey".into(),
                cli.get("sse_c_key").cloned().unwrap_or(Value::Null),
            );
        }
    }

    fn sse_c_copy_source(p: &mut Map, cli: &Args) {
        if cli.truthy("sse_c_copy_source") {
            set(
                p,
                "CopySourceSSECustomerAlgorithm",
                cli.get("sse_c_copy_source").unwrap(),
            );
            p.insert(
                "CopySourceSSECustomerKey".into(),
                cli.get("sse_c_copy_source_key")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
    }

    fn request_payer(p: &mut Map, cli: &Args) {
        if cli.truthy("request_payer") {
            set(p, "RequestPayer", cli.get("request_payer").unwrap());
        }
    }

    fn checksum_mode(p: &mut Map, cli: &Args) {
        if cli.truthy("checksum_mode") {
            set(p, "ChecksumMode", cli.get("checksum_mode").unwrap());
        }
    }

    fn checksum_algorithm(p: &mut Map, cli: &Args) {
        if cli.truthy("checksum_algorithm") {
            set(
                p,
                "ChecksumAlgorithm",
                cli.get("checksum_algorithm").unwrap(),
            );
        }
    }

    fn no_overwrite(p: &mut Map, cli: &Args) {
        if cli.truthy("no_overwrite") {
            p.insert("IfNoneMatch".into(), Value::str("*"));
        }
    }

    pub(crate) fn put_object(p: &mut Map, cli: &Args) -> Result<(), Fail> {
        general(p, cli)?;
        metadata(p, cli);
        tagging(p, cli);
        sse(p, cli);
        sse_c(p, cli);
        request_payer(p, cli);
        checksum_algorithm(p, cli);
        no_overwrite(p, cli);
        Ok(())
    }

    pub(crate) fn get_object(p: &mut Map, cli: &Args) -> Result<(), Fail> {
        sse_c(p, cli);
        request_payer(p, cli);
        checksum_mode(p, cli);
        Ok(())
    }

    pub(crate) fn copy_object(p: &mut Map, cli: &Args) -> Result<(), Fail> {
        general(p, cli)?;
        if cli.truthy("metadata_directive") {
            set(
                p,
                "MetadataDirective",
                cli.get("metadata_directive").unwrap(),
            );
        }
        metadata(p, cli);
        if p.get("Metadata").is_some_and(Value::is_truthy)
            && !p.get("MetadataDirective").is_some_and(Value::is_truthy)
        {
            p.insert("MetadataDirective".into(), Value::str("REPLACE"));
        }
        sse(p, cli);
        sse_c(p, cli);
        sse_c_copy_source(p, cli);
        request_payer(p, cli);
        checksum_algorithm(p, cli);
        no_overwrite(p, cli);
        Ok(())
    }

    pub(crate) fn head_object(p: &mut Map, cli: &Args) {
        sse_c(p, cli);
        request_payer(p, cli);
        checksum_mode(p, cli);
    }

    pub(crate) fn head_object_with_copy_source_sse(p: &mut Map, cli: &Args) {
        if cli.truthy("sse_c_copy_source") {
            set(
                p,
                "SSECustomerAlgorithm",
                cli.get("sse_c_copy_source").unwrap(),
            );
            p.insert(
                "SSECustomerKey".into(),
                cli.get("sse_c_copy_source_key")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        request_payer(p, cli);
    }

    pub(crate) fn delete_object(p: &mut Map, cli: &Args) -> Result<(), Fail> {
        request_payer(p, cli);
        Ok(())
    }

    pub(crate) fn request_payer_only(p: &mut Map, cli: &Args) {
        request_payer(p, cli);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_readable_size(1.0), "1 Byte");
        assert_eq!(human_readable_size(10.0), "10 Bytes");
        assert_eq!(human_readable_size(1024.0), "1.0 KiB");
        assert_eq!(human_readable_size(1_000_000.0), "976.6 KiB");
        assert_eq!(human_readable_size(1_048_575.0), "1.0 MiB");
        assert_eq!(human_readable_size(3_145_745.0), "3.0 MiB");
        assert_eq!(human_readable_to_int("10MB"), Ok(10 * 1024 * 1024));
        assert_eq!(human_readable_to_int("1kib"), Ok(1024));
        assert_eq!(human_readable_to_int("123"), Ok(123));
        assert!(human_readable_to_int("1x").is_err());
    }

    #[test]
    fn bucket_keys() {
        assert_eq!(
            find_bucket_key("b/k/x").unwrap(),
            ("b".to_string(), "k/x".to_string())
        );
        assert_eq!(
            find_bucket_key("arn:aws:s3:us-west-2:123456789012:accesspoint/ap/k").unwrap(),
            (
                "arn:aws:s3:us-west-2:123456789012:accesspoint/ap".to_string(),
                "k".to_string()
            )
        );
    }

    #[test]
    fn content_types() {
        assert_eq!(
            guess_content_type("/a/b.txt").as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            guess_content_type("/a/b.tar.gz").as_deref(),
            Some("application/x-tar")
        );
        assert_eq!(
            guess_content_type("/a/b.tgz").as_deref(),
            Some("application/x-tar")
        );
        assert_eq!(
            guess_content_type("/a/b.JPG").as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(guess_content_type("/a/.bashrc"), None);
        assert_eq!(guess_content_type("/a/b"), None);
    }

    #[test]
    fn paths() {
        assert_eq!(normpath("/a/./b/../c/"), "/a/c");
        assert_eq!(dirname("/a/b/c"), "/a/b");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(relpath("/a/b/c", "/a/d"), "../b/c");
        assert_eq!(join("a", "b"), "a/b");
    }

    #[test]
    fn timestamps() {
        assert_eq!(
            parse_timestamp("2023-11-14T22:13:20+00:00").unwrap().secs(),
            1_700_000_000
        );
        assert_eq!(
            parse_timestamp("2023-11-15T07:13:20.500000+09:00")
                .unwrap()
                .micros,
            1_700_000_000_500_000
        );
    }
}

/// `fnmatch.translate` (CPython 3.14) as a Rust regex matching the whole
/// string.
pub(crate) fn fnmatch_translate(pat: &str) -> String {
    let p: Vec<char> = pat.chars().collect();
    let n = p.len();
    let mut res: Vec<String> = vec![];
    let mut i = 0;
    while i < n {
        let c = p[i];
        i += 1;
        if c == '*' {
            res.push(".*".to_string());
            while i < n && p[i] == '*' {
                i += 1;
            }
        } else if c == '?' {
            res.push(".".to_string());
        } else if c == '[' {
            let mut j = i;
            if j < n && p[j] == '!' {
                j += 1;
            }
            if j < n && p[j] == ']' {
                j += 1;
            }
            while j < n && p[j] != ']' {
                j += 1;
            }
            if j >= n {
                res.push("\\[".to_string());
                continue;
            }
            let s = |a: usize, b: usize| -> String { p[a..b].iter().collect() };
            let mut stuff = s(i, j);
            if !stuff.contains('-') {
                stuff = stuff.replace('\\', "\\\\");
            } else {
                let mut chunks: Vec<String> = vec![];
                let mut k = if p[i] == '!' { i + 2 } else { i + 1 };
                let mut ii = i;
                loop {
                    let found = (k..j).find(|&x| x < n && p[x] == '-');
                    let Some(kk) = found else {
                        break;
                    };
                    chunks.push(s(ii, kk));
                    ii = kk + 1;
                    k = kk + 3;
                }
                let chunk = s(ii, j);
                if !chunk.is_empty() {
                    chunks.push(chunk);
                } else if let Some(last) = chunks.last_mut() {
                    last.push('-');
                }
                let mut k = chunks.len().saturating_sub(1);
                while k > 0 {
                    let prev_last = chunks[k - 1].chars().last();
                    let cur_first = chunks[k].chars().next();
                    if let (Some(a), Some(b)) = (prev_last, cur_first)
                        && a > b
                    {
                        let mut merged: String = chunks[k - 1]
                            .chars()
                            .take(chunks[k - 1].chars().count() - 1)
                            .collect();
                        merged.extend(chunks[k].chars().skip(1));
                        chunks[k - 1] = merged;
                        chunks.remove(k);
                    }
                    k -= 1;
                }
                stuff = chunks
                    .iter()
                    .map(|c| c.replace('\\', "\\\\").replace('-', "\\-"))
                    .collect::<Vec<_>>()
                    .join("-");
            }
            i = j + 1;
            if stuff.is_empty() {
                res.push("[^\\s\\S]".to_string());
            } else if stuff == "!" {
                res.push(".".to_string());
            } else {
                let mut esc = String::new();
                for ch in stuff.chars() {
                    if matches!(ch, '&' | '~' | '|' | '[') {
                        esc.push('\\');
                    }
                    esc.push(ch);
                }
                let mut stuff = esc;
                if let Some(rest) = stuff.strip_prefix('!') {
                    stuff = format!("^{rest}");
                } else if stuff.starts_with('^') {
                    stuff = format!("\\{stuff}");
                }
                res.push(format!("[{stuff}]"));
            }
        } else {
            res.push(regex::escape(&c.to_string()));
        }
    }
    format!("^(?s:{})\\z", res.concat())
}

/// `fnmatch.fnmatch` on POSIX (case-sensitive).
pub(crate) fn fnmatch(name: &str, pat: &str) -> bool {
    Regex::new(&fnmatch_translate(pat)).is_ok_and(|r| r.is_match(name))
}

#[cfg(test)]
mod fnmatch_tests {
    use super::fnmatch;

    #[test]
    fn patterns() {
        assert!(fnmatch("/a/b/c.txt", "/a/*.txt"));
        assert!(fnmatch("/a/b/c.txt", "/a/*"));
        assert!(!fnmatch("/a/b/c.txt", "/a/*.log"));
        assert!(fnmatch("x.json", "*.[jh]*"));
        assert!(!fnmatch("x.bin", "*.[jh]*"));
        assert!(fnmatch("a", "[!b]"));
        assert!(fnmatch("[", "["));
        assert!(fnmatch("a-", "a[-]"));
        assert!(fnmatch("ab", "a?"));
        assert!(!fnmatch("A.txt", "a.txt"));
    }
}
