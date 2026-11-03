//! A stateful in-memory S3 endpoint for the `s3` scenarios: buckets and
//! objects live in memory, every request is recorded, and the operations
//! the high-level `aws s3` commands use are answered the way S3 answers
//! them (path-style or virtual-hosted addressing under `*.localhost`).

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::server::Recorded;

/// The instant every write is stamped with (2030-01-01T00:00:00Z), later
/// than any local fixture so `sync` comparisons are stable.
pub const WRITE_TIME: i64 = 1_893_456_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Object {
    pub body: Vec<u8>,
    pub last_modified: i64,
    pub etag: String,
    /// Stored system and user metadata headers (lowercase names).
    pub headers: BTreeMap<String, String>,
    pub tags: Vec<(String, String)>,
}

impl Object {
    pub fn new(body: Vec<u8>, last_modified: i64) -> Object {
        let etag = format!("\"{}\"", md5_hex(&body));
        Object {
            body,
            last_modified,
            etag,
            headers: BTreeMap::new(),
            tags: vec![],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bucket {
    pub objects: BTreeMap<String, Object>,
    pub website: Option<String>,
    pub created: i64,
}

#[derive(Debug, Default)]
struct Upload {
    bucket: String,
    key: String,
    headers: BTreeMap<String, String>,
    parts: BTreeMap<u32, Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct S3State {
    pub buckets: BTreeMap<String, Bucket>,
    uploads: BTreeMap<String, Upload>,
}

struct Shared {
    state: Mutex<S3State>,
    log: Mutex<Vec<Recorded>>,
    host_suffix: String,
}

pub struct S3Stub {
    pub port: u16,
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl S3Stub {
    pub async fn start(host: &str) -> Result<S3Stub> {
        let v4 = TcpListener::bind("127.0.0.1:0").await?;
        let port = v4.local_addr()?.port();
        let shared = Arc::new(Shared {
            state: Mutex::new(S3State::default()),
            log: Mutex::new(vec![]),
            host_suffix: format!(".{host}:{port}"),
        });
        let mut tasks = vec![tokio::spawn(accept_loop(v4, shared.clone()))];
        if let Ok(v6) =
            TcpListener::bind(SocketAddr::from(([0u16, 0, 0, 0, 0, 0, 0, 1], port))).await
        {
            tasks.push(tokio::spawn(accept_loop(v6, shared.clone())));
        }
        Ok(S3Stub {
            port,
            shared,
            tasks,
        })
    }

    pub fn set_state(&self, buckets: BTreeMap<String, Bucket>) {
        let mut st = self.shared.state.lock().unwrap();
        *st = S3State {
            buckets,
            ..Default::default()
        };
    }

    pub fn take_log(&self) -> Vec<Recorded> {
        std::mem::take(&mut *self.shared.log.lock().unwrap())
    }

    /// The buckets and objects, one line per object.
    pub fn dump(&self) -> String {
        let st = self.shared.state.lock().unwrap();
        let mut out = String::new();
        for (name, b) in &st.buckets {
            let _ = writeln!(out, "bucket {name}");
            if let Some(w) = &b.website {
                let _ = writeln!(out, "  website {w}");
            }
            for (k, o) in &b.objects {
                let _ = writeln!(
                    out,
                    "  {k} size={} md5={} etag={} mtime={}",
                    o.body.len(),
                    md5_hex(&o.body),
                    o.etag,
                    o.last_modified
                );
                for (h, v) in &o.headers {
                    let _ = writeln!(out, "    {h}: {v}");
                }
                for (tk, tv) in &o.tags {
                    let _ = writeln!(out, "    tag {tk}={tv}");
                }
            }
        }
        for u in st.uploads.values() {
            let _ = writeln!(out, "pending upload {}/{}", u.bucket, u.key);
        }
        out
    }
}

impl Drop for S3Stub {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(req, shared.clone()));
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}

pub fn md5_hex(body: &[u8]) -> String {
    use base64::Engine;
    let b64 = awsr::session::content_md5(body);
    hex::encode(
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap_or_default(),
    )
}

/// `days_from_civil` inverse: (year, month, day) of a day count.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn split_time(t: i64) -> (i64, u32, u32, i64, i64, i64, i64) {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    let wday = (days + 4).rem_euclid(7);
    (y, m, d, secs / 3600, secs / 60 % 60, secs % 60, wday)
}

pub fn iso8601(t: i64) -> String {
    let (y, m, d, hh, mm, ss, _) = split_time(t);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.000Z")
}

fn http_date(t: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d, hh, mm, ss, wd) = split_time(t);
    format!(
        "{}, {d:02} {} {y:04} {hh:02}:{mm:02}:{ss:02} GMT",
        DAYS[wd as usize],
        MONTHS[(m - 1) as usize]
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .map_err(|_| ())
                .and_then(|h| u8::from_str_radix(h, 16).map_err(|_| ()))
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_decode(s: &str) -> String {
    percent_decode(&s.replace('+', " "))
}

/// Percent-encode like `urllib.parse.quote(s, safe='/')`... S3 uses the
/// RFC 3986 unreserved set plus `/` for `encoding-type=url`.
fn url_encode_key(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn xml(status: u16, body: String) -> Reply {
        Reply {
            status,
            headers: vec![("Content-Type".into(), "application/xml".into())],
            body: format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{body}").into_bytes(),
        }
    }

    fn empty(status: u16) -> Reply {
        Reply {
            status,
            headers: vec![],
            body: vec![],
        }
    }

    fn error(status: u16, code: &str, message: &str) -> Reply {
        Reply::xml(
            status,
            format!(
                "<Error><Code>{code}</Code><Message>{}</Message><RequestId>difftest</RequestId></Error>",
                xml_escape(message)
            ),
        )
    }
}

/// Decode an `aws-chunked` body (chunk headers with optional signatures,
/// trailers ignored).
fn decode_aws_chunked(body: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    let mut i = 0;
    while i < body.len() {
        let Some(nl) = body[i..].windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let line = String::from_utf8_lossy(&body[i..i + nl]).into_owned();
        let size_hex = line.split(';').next().unwrap_or("0");
        let size = usize::from_str_radix(size_hex.trim(), 16).unwrap_or(0);
        i += nl + 2;
        if size == 0 {
            break;
        }
        out.extend_from_slice(&body[i..(i + size).min(body.len())]);
        i += size + 2;
    }
    out
}

const STORED_HEADERS: &[&str] = &[
    "content-type",
    "cache-control",
    "content-disposition",
    "content-encoding",
    "content-language",
    "expires",
    "x-amz-storage-class",
    "x-amz-server-side-encryption",
    "x-amz-server-side-encryption-aws-kms-key-id",
    "x-amz-website-redirect-location",
    "x-amz-acl",
    "x-amz-grant-read",
    "x-amz-grant-full-control",
    "x-amz-grant-read-acp",
    "x-amz-grant-write-acp",
    "x-amz-server-side-encryption-customer-algorithm",
];

fn stored_headers(headers: &[(String, String)]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in headers {
        if STORED_HEADERS.contains(&k.as_str()) || k.starts_with("x-amz-meta-") {
            let v = if k == "content-encoding" {
                v.split(',')
                    .map(str::trim)
                    .filter(|e| *e != "aws-chunked")
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                v.clone()
            };
            if !v.is_empty() {
                out.insert(k.clone(), v);
            }
        }
    }
    out
}

fn object_headers(o: &Object) -> Vec<(String, String)> {
    let mut h = vec![
        ("Last-Modified".to_string(), http_date(o.last_modified)),
        ("ETag".to_string(), o.etag.clone()),
        ("Accept-Ranges".to_string(), "bytes".to_string()),
    ];
    if !o.headers.contains_key("content-type") {
        h.push(("Content-Type".into(), "binary/octet-stream".into()));
    }
    for (k, v) in &o.headers {
        if k.starts_with("x-amz-acl") || k.starts_with("x-amz-grant") {
            continue;
        }
        h.push((k.clone(), v.clone()));
    }
    h
}

fn get_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Echo request checksum headers the way S3 returns them.
fn checksum_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(k, _)| k.starts_with("x-amz-checksum-") && k != "x-amz-checksum-algorithm")
        .cloned()
        .collect()
}

async fn handle(
    req: Request<Incoming>,
    shared: Arc<Shared>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes().to_vec())
        .unwrap_or_default();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let host = get_header(&headers, "host").unwrap_or("").to_string();
    shared.log.lock().unwrap().push(Recorded {
        method: parts.method.to_string(),
        target: target.clone(),
        headers: parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
            .collect(),
        body: body.clone(),
    });
    let body =
        if get_header(&headers, "content-encoding").is_some_and(|e| e.contains("aws-chunked")) {
            decode_aws_chunked(&body)
        } else {
            body
        };
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let mut q: Vec<(String, String)> = vec![];
    for p in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        q.push((query_decode(k), query_decode(v)));
    }
    let (bucket, key) = match host.strip_suffix(&shared.host_suffix) {
        Some(b) => (b.to_string(), percent_decode(path.trim_start_matches('/'))),
        None => {
            let p = path.trim_start_matches('/');
            let (b, k) = p.split_once('/').unwrap_or((p, ""));
            (percent_decode(b), percent_decode(k))
        }
    };
    let reply = {
        let mut st = shared.state.lock().unwrap();
        dispatch(
            &mut st,
            parts.method.as_str(),
            &bucket,
            &key,
            &q,
            &headers,
            body,
        )
    };
    let head = parts.method == hyper::Method::HEAD;
    let mut out = Response::new(Full::new(Bytes::from(if head {
        vec![]
    } else {
        reply.body.clone()
    })));
    *out.status_mut() = hyper::StatusCode::from_u16(reply.status).unwrap();
    for (k, v) in &reply.headers {
        if let (Ok(n), Ok(v)) = (
            hyper::header::HeaderName::from_bytes(k.as_bytes()),
            hyper::header::HeaderValue::from_str(v),
        ) {
            out.headers_mut().append(n, v);
        }
    }
    out.headers_mut().insert(
        "x-amz-request-id",
        hyper::header::HeaderValue::from_static("difftest"),
    );
    if head {
        out.headers_mut().insert(
            hyper::header::CONTENT_LENGTH,
            hyper::header::HeaderValue::from(reply.body.len()),
        );
    }
    Ok(out)
}

fn has(q: &[(String, String)], k: &str) -> bool {
    q.iter().any(|(n, _)| n == k)
}

fn qget<'a>(q: &'a [(String, String)], k: &str) -> Option<&'a str> {
    q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

fn no_bucket(bucket: &str) -> Reply {
    let mut r = Reply::error(404, "NoSuchBucket", "The specified bucket does not exist");
    let _ = bucket;
    r.headers
        .push(("x-amz-bucket-region".into(), "us-east-1".into()));
    r
}

fn dispatch(
    st: &mut S3State,
    method: &str,
    bucket: &str,
    key: &str,
    q: &[(String, String)],
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Reply {
    if bucket.is_empty() {
        return list_buckets(st, q);
    }
    if key.is_empty() {
        return bucket_op(st, method, bucket, q, headers, body);
    }
    if !st.buckets.contains_key(bucket) {
        return no_bucket(bucket);
    }
    match method {
        "HEAD" | "GET" => {
            let Some(o) = st.buckets[bucket].objects.get(key) else {
                if method == "HEAD" {
                    return Reply::empty(404);
                }
                return Reply::error(404, "NoSuchKey", "The specified key does not exist.");
            };
            if has(q, "tagging") {
                let mut s = String::from("<Tagging><TagSet>");
                for (k, v) in &o.tags {
                    let _ = write!(
                        s,
                        "<Tag><Key>{}</Key><Value>{}</Value></Tag>",
                        xml_escape(k),
                        xml_escape(v)
                    );
                }
                s.push_str("</TagSet></Tagging>");
                return Reply::xml(200, s);
            }
            if let Some(m) = get_header(headers, "if-match")
                && m != o.etag
            {
                return Reply::error(
                    412,
                    "PreconditionFailed",
                    "At least one of the pre-conditions you specified did not hold",
                );
            }
            let mut h = object_headers(o);
            if let Some(r) = get_header(headers, "range") {
                let total = o.body.len();
                let spec = r.trim_start_matches("bytes=");
                let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
                let start: usize = a.parse().unwrap_or(0);
                let end: usize = if b.is_empty() {
                    total.saturating_sub(1)
                } else {
                    b.parse::<usize>().unwrap_or(0).min(total.saturating_sub(1))
                };
                let slice = o.body.get(start..=end).unwrap_or(&[]).to_vec();
                h.push((
                    "Content-Range".into(),
                    format!("bytes {start}-{end}/{total}"),
                ));
                return Reply {
                    status: 206,
                    headers: h,
                    body: slice,
                };
            }
            Reply {
                status: 200,
                headers: h,
                body: o.body.clone(),
            }
        }
        "PUT" => {
            if let Some(src) = get_header(headers, "x-amz-copy-source") {
                return copy(st, bucket, key, q, headers, src);
            }
            if let (Some(n), Some(id)) = (qget(q, "partNumber"), qget(q, "uploadId")) {
                let Some(u) = st.uploads.get_mut(id) else {
                    return Reply::error(
                        404,
                        "NoSuchUpload",
                        "The specified upload does not exist.",
                    );
                };
                let etag = format!("\"{}\"", md5_hex(&body));
                u.parts.insert(n.parse().unwrap_or(0), body);
                let mut h = vec![("ETag".to_string(), etag)];
                h.extend(checksum_headers(headers));
                return Reply {
                    status: 200,
                    headers: h,
                    body: vec![],
                };
            }
            let b = st.buckets.get_mut(bucket).unwrap();
            if get_header(headers, "if-none-match") == Some("*") && b.objects.contains_key(key) {
                return Reply::error(
                    412,
                    "PreconditionFailed",
                    "At least one of the pre-conditions you specified did not hold",
                );
            }
            let mut o = Object::new(body, WRITE_TIME);
            o.headers = stored_headers(headers);
            if let Some(t) = get_header(headers, "x-amz-tagging") {
                o.tags = parse_tagging(t);
            }
            let etag = o.etag.clone();
            b.objects.insert(key.to_string(), o);
            let mut h = vec![("ETag".to_string(), etag)];
            h.extend(checksum_headers(headers));
            Reply {
                status: 200,
                headers: h,
                body: vec![],
            }
        }
        "POST" => {
            if has(q, "uploads") {
                let id = format!(
                    "upload-{}",
                    &md5_hex(format!("{bucket}/{key}").as_bytes())[..12]
                );
                st.uploads.insert(
                    id.clone(),
                    Upload {
                        bucket: bucket.to_string(),
                        key: key.to_string(),
                        headers: stored_headers(headers),
                        parts: BTreeMap::new(),
                    },
                );
                return Reply::xml(
                    200,
                    format!(
                        "<InitiateMultipartUploadResult><Bucket>{}</Bucket><Key>{}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>",
                        xml_escape(bucket),
                        xml_escape(key)
                    ),
                );
            }
            if let Some(id) = qget(q, "uploadId") {
                let Some(u) = st.uploads.remove(id) else {
                    return Reply::error(
                        404,
                        "NoSuchUpload",
                        "The specified upload does not exist.",
                    );
                };
                let mut data = vec![];
                let mut md5s = vec![];
                for p in u.parts.values() {
                    data.extend_from_slice(p);
                    md5s.extend(hex::decode(md5_hex(p)).unwrap_or_default());
                }
                let n = u.parts.len();
                let mut o = Object::new(data, WRITE_TIME);
                o.etag = format!("\"{}-{n}\"", md5_hex(&md5s));
                o.headers = u.headers;
                let etag = o.etag.clone();
                if let Some(b) = st.buckets.get_mut(bucket) {
                    b.objects.insert(key.to_string(), o);
                }
                return Reply::xml(
                    200,
                    format!(
                        "<CompleteMultipartUploadResult><Bucket>{}</Bucket><Key>{}</Key><ETag>{}</ETag></CompleteMultipartUploadResult>",
                        xml_escape(bucket),
                        xml_escape(key),
                        xml_escape(&etag)
                    ),
                );
            }
            Reply::error(400, "InvalidRequest", "unsupported POST")
        }
        "DELETE" => {
            if let Some(id) = qget(q, "uploadId") {
                st.uploads.remove(id);
                return Reply::empty(204);
            }
            st.buckets.get_mut(bucket).unwrap().objects.remove(key);
            Reply::empty(204)
        }
        _ => Reply::error(405, "MethodNotAllowed", "method not allowed"),
    }
}

fn parse_tagging(t: &str) -> Vec<(String, String)> {
    t.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (query_decode(k), query_decode(v))
        })
        .collect()
}

fn copy(
    st: &mut S3State,
    bucket: &str,
    key: &str,
    q: &[(String, String)],
    headers: &[(String, String)],
    src: &str,
) -> Reply {
    let src = percent_decode(src.trim_start_matches('/'));
    let (sb, sk) = src.split_once('/').unwrap_or((&src, ""));
    let sk = sk.split("?versionId=").next().unwrap_or(sk);
    let Some(source) = st.buckets.get(sb).and_then(|b| b.objects.get(sk)).cloned() else {
        return Reply::error(404, "NoSuchKey", "The specified key does not exist.");
    };
    if let Some(m) = get_header(headers, "x-amz-copy-source-if-match")
        && m != source.etag
    {
        return Reply::error(
            412,
            "PreconditionFailed",
            "At least one of the pre-conditions you specified did not hold",
        );
    }
    if let (Some(n), Some(id)) = (qget(q, "partNumber"), qget(q, "uploadId")) {
        let range = get_header(headers, "x-amz-copy-source-range").unwrap_or("");
        let spec = range.trim_start_matches("bytes=");
        let (a, b) = spec.split_once('-').unwrap_or(("0", ""));
        let start: usize = a.parse().unwrap_or(0);
        let end: usize = b
            .parse()
            .unwrap_or(source.body.len().saturating_sub(1))
            .min(source.body.len().saturating_sub(1));
        let data = source.body.get(start..=end).unwrap_or(&[]).to_vec();
        let etag = format!("\"{}\"", md5_hex(&data));
        let Some(u) = st.uploads.get_mut(id) else {
            return Reply::error(404, "NoSuchUpload", "The specified upload does not exist.");
        };
        u.parts.insert(n.parse().unwrap_or(0), data);
        return Reply::xml(
            200,
            format!(
                "<CopyPartResult><LastModified>{}</LastModified><ETag>{}</ETag></CopyPartResult>",
                iso8601(WRITE_TIME),
                xml_escape(&etag)
            ),
        );
    }
    let Some(b) = st.buckets.get_mut(bucket) else {
        return no_bucket(bucket);
    };
    if get_header(headers, "if-none-match") == Some("*") && b.objects.contains_key(key) {
        return Reply::error(
            412,
            "PreconditionFailed",
            "At least one of the pre-conditions you specified did not hold",
        );
    }
    let mut o = Object::new(source.body.clone(), WRITE_TIME);
    o.etag = source.etag.clone();
    if get_header(headers, "x-amz-metadata-directive") == Some("REPLACE") {
        o.headers = stored_headers(headers);
    } else {
        o.headers = source.headers.clone();
        for (k, v) in stored_headers(headers) {
            if !k.starts_with("x-amz-meta-") && k != "content-type" {
                o.headers.insert(k, v);
            }
        }
    }
    if get_header(headers, "x-amz-tagging-directive") == Some("REPLACE") {
        o.tags = get_header(headers, "x-amz-tagging")
            .map(parse_tagging)
            .unwrap_or_default();
    } else {
        o.tags = source.tags.clone();
    }
    let etag = o.etag.clone();
    b.objects.insert(key.to_string(), o);
    Reply::xml(
        200,
        format!(
            "<CopyObjectResult><LastModified>{}</LastModified><ETag>{}</ETag></CopyObjectResult>",
            iso8601(WRITE_TIME),
            xml_escape(&etag)
        ),
    )
}

fn list_buckets(st: &S3State, q: &[(String, String)]) -> Reply {
    let prefix = qget(q, "prefix").unwrap_or("");
    let max: usize = qget(q, "max-buckets")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let after = qget(q, "continuation-token").unwrap_or("");
    let names: Vec<&String> = st
        .buckets
        .keys()
        .filter(|n| n.starts_with(prefix) && n.as_str() > after)
        .collect();
    let mut s = String::from("<ListAllMyBucketsResult><Owner><ID>difftest</ID></Owner><Buckets>");
    for n in names.iter().take(max) {
        let _ = write!(
            s,
            "<Bucket><Name>{}</Name><CreationDate>{}</CreationDate><BucketRegion>us-east-1</BucketRegion></Bucket>",
            xml_escape(n),
            iso8601(st.buckets[*n].created)
        );
    }
    s.push_str("</Buckets>");
    if names.len() > max {
        let _ = write!(
            s,
            "<ContinuationToken>{}</ContinuationToken>",
            xml_escape(names[max - 1])
        );
    }
    if !prefix.is_empty() {
        let _ = write!(s, "<Prefix>{}</Prefix>", xml_escape(prefix));
    }
    s.push_str("</ListAllMyBucketsResult>");
    Reply::xml(200, s)
}

fn bucket_op(
    st: &mut S3State,
    method: &str,
    bucket: &str,
    q: &[(String, String)],
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Reply {
    let _ = headers;
    match method {
        "PUT" if has(q, "website") => {
            let Some(b) = st.buckets.get_mut(bucket) else {
                return no_bucket(bucket);
            };
            b.website = Some(String::from_utf8_lossy(&body).replace('\n', ""));
            Reply::empty(200)
        }
        "PUT" => {
            if st.buckets.contains_key(bucket) {
                return Reply::error(
                    409,
                    "BucketAlreadyOwnedByYou",
                    "Your previous request to create the named bucket succeeded and you already own it.",
                );
            }
            st.buckets.insert(
                bucket.to_string(),
                Bucket {
                    created: WRITE_TIME,
                    ..Default::default()
                },
            );
            Reply {
                status: 200,
                headers: vec![("Location".into(), format!("/{bucket}"))],
                body: vec![],
            }
        }
        "DELETE" => {
            let Some(b) = st.buckets.get(bucket) else {
                return no_bucket(bucket);
            };
            if !b.objects.is_empty() {
                return Reply::error(
                    409,
                    "BucketNotEmpty",
                    "The bucket you tried to delete is not empty",
                );
            }
            st.buckets.remove(bucket);
            Reply::empty(204)
        }
        "HEAD" => {
            if st.buckets.contains_key(bucket) {
                Reply {
                    status: 200,
                    headers: vec![("x-amz-bucket-region".into(), "us-east-1".into())],
                    body: vec![],
                }
            } else {
                Reply::empty(404)
            }
        }
        "POST" if has(q, "delete") => {
            let Some(b) = st.buckets.get_mut(bucket) else {
                return no_bucket(bucket);
            };
            let text = String::from_utf8_lossy(&body).into_owned();
            let mut s = String::from("<DeleteResult>");
            for part in text.split("<Key>").skip(1) {
                let k = part.split("</Key>").next().unwrap_or("");
                let k = k
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&amp;", "&");
                b.objects.remove(&k);
                let _ = write!(s, "<Deleted><Key>{}</Key></Deleted>", xml_escape(&k));
            }
            s.push_str("</DeleteResult>");
            Reply::xml(200, s)
        }
        "GET" if has(q, "location") => {
            if !st.buckets.contains_key(bucket) {
                return no_bucket(bucket);
            }
            Reply::xml(
                200,
                "<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"/>".into(),
            )
        }
        "GET" => {
            let Some(b) = st.buckets.get(bucket) else {
                return no_bucket(bucket);
            };
            list_objects_v2(bucket, b, q)
        }
        _ => Reply::error(405, "MethodNotAllowed", "method not allowed"),
    }
}

fn list_objects_v2(name: &str, b: &Bucket, q: &[(String, String)]) -> Reply {
    let prefix = qget(q, "prefix").unwrap_or("");
    let delimiter = qget(q, "delimiter").unwrap_or("");
    let max: usize = qget(q, "max-keys")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let token = qget(q, "continuation-token")
        .or_else(|| qget(q, "start-after"))
        .unwrap_or("");
    let url = qget(q, "encoding-type") == Some("url");
    let enc = |s: &str| {
        if url {
            url_encode_key(s)
        } else {
            xml_escape(s)
        }
    };
    // (key or common prefix, is_prefix)
    let mut entries: Vec<(String, bool)> = vec![];
    for k in b.objects.keys() {
        if !k.starts_with(prefix) || k.as_str() <= token {
            continue;
        }
        if !delimiter.is_empty()
            && let Some(pos) = k[prefix.len()..].find(delimiter)
        {
            let cp = k[..prefix.len() + pos + delimiter.len()].to_string();
            if cp.as_str() <= token {
                continue;
            }
            if entries.last().is_none_or(|(e, p)| !(*p && *e == cp)) {
                entries.push((cp, true));
            }
            continue;
        }
        entries.push((k.clone(), false));
    }
    let truncated = entries.len() > max;
    let page: Vec<&(String, bool)> = entries.iter().take(max).collect();
    let mut s = format!(
        "<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>{}</Name><Prefix>{}</Prefix>",
        xml_escape(name),
        enc(prefix)
    );
    if let Some(t) = qget(q, "continuation-token") {
        let _ = write!(
            s,
            "<ContinuationToken>{}</ContinuationToken>",
            xml_escape(t)
        );
    }
    let _ = write!(
        s,
        "<KeyCount>{}</KeyCount><MaxKeys>{max}</MaxKeys>",
        page.len()
    );
    if !delimiter.is_empty() {
        let _ = write!(s, "<Delimiter>{}</Delimiter>", enc(delimiter));
    }
    if url {
        s.push_str("<EncodingType>url</EncodingType>");
    }
    let _ = write!(s, "<IsTruncated>{truncated}</IsTruncated>");
    if truncated && let Some((last, _)) = page.last() {
        // The token sorts after every key under a common prefix.
        let t = if last.ends_with(delimiter) && !delimiter.is_empty() {
            format!("{last}\u{10ffff}")
        } else {
            last.clone()
        };
        let _ = write!(
            s,
            "<NextContinuationToken>{}</NextContinuationToken>",
            xml_escape(&t)
        );
    }
    for (k, is_prefix) in &page {
        if *is_prefix {
            continue;
        }
        let o = &b.objects[k];
        let class = o
            .headers
            .get("x-amz-storage-class")
            .cloned()
            .unwrap_or_else(|| "STANDARD".to_string());
        let _ = write!(
            s,
            "<Contents><Key>{}</Key><LastModified>{}</LastModified><ETag>{}</ETag><Size>{}</Size><StorageClass>{class}</StorageClass></Contents>",
            enc(k),
            iso8601(o.last_modified),
            xml_escape(&o.etag),
            o.body.len()
        );
    }
    for (k, is_prefix) in &page {
        if *is_prefix {
            let _ = write!(
                s,
                "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                enc(k)
            );
        }
    }
    s.push_str("</ListBucketResult>");
    Reply::xml(200, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(iso8601(WRITE_TIME), "2030-01-01T00:00:00.000Z");
        assert_eq!(http_date(WRITE_TIME), "Tue, 01 Jan 2030 00:00:00 GMT");
    }

    #[test]
    fn chunked() {
        assert_eq!(
            decode_aws_chunked(b"3\r\nabc\r\n0\r\nx-amz-checksum-crc32:AAAA\r\n\r\n"),
            b"abc"
        );
    }
}
