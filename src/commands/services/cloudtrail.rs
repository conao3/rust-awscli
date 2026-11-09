//! `cloudtrail validate-logs` (`awscli/customizations/cloudtrail/validation.py`)
//! and `cloudtrail verify-query-results` (`verifyqueryresults.py`).

mod verify;

use std::cmp::Ordering;
use std::collections::HashMap;

use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
use sha2::{Digest as _, Sha256};

use super::basic::{self, ArgDef};
use super::exec::Client;
use crate::commands::s3::client::{CallError, Client as S3Client};
use crate::driver::{ClientConfig, Context, client_error};
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "validate-logs" => validate_logs(ctx, args),
        "verify-query-results" => verify::verify_query_results(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

fn general(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

/// `parse_date` (`dateutil.parser.parse`) for the ISO 8601 forms, then
/// `normalize_date` (the time zone is replaced, not converted).
fn parse_date(s: &str) -> Option<NaiveDateTime> {
    let t = s.trim();
    let (body, _) = split_offset(t);
    for f in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y%m%dT%H%M%S",
    ] {
        if let Ok(d) = NaiveDateTime::parse_from_str(body, f) {
            return Some(d);
        }
    }
    for f in ["%Y-%m-%d", "%Y%m%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(body, f) {
            return d.and_hms_opt(0, 0, 0);
        }
    }
    None
}

/// Strip a trailing `Z` or `±HH:MM` / `±HHMM` offset.
fn split_offset(s: &str) -> (&str, Option<&str>) {
    if let Some(b) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return (b, Some("Z"));
    }
    if s.len() > 10
        && let Some(i) = s[10..].rfind(['+', '-']).map(|i| i + 10)
    {
        let off = &s[i + 1..];
        if off.len() >= 2 && off.chars().all(|c| c.is_ascii_digit() || c == ':') {
            return (&s[..i], Some(&s[i..]));
        }
    }
    (s, None)
}

/// `type(value).__name__` of a JSON value.
fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
        Value::List(_) => "list",
        Value::Map(_) => "dict",
        _ => "bytes",
    }
}

/// A `datetime` as `dateutil.parser.parse` returns it: the wall clock and
/// the UTC offset in seconds (`None` for a naive value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PyDate {
    naive: NaiveDateTime,
    offset: Option<i32>,
}

fn parse_offset(off: &str) -> Option<i32> {
    if off.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let sign = if off.starts_with('-') { -1 } else { 1 };
    let digits: String = off[1..].chars().filter(|c| c.is_ascii_digit()).collect();
    let (h, m) = match digits.len() {
        2 => (digits.parse::<i32>().ok()?, 0),
        4 => (
            digits[..2].parse::<i32>().ok()?,
            digits[2..].parse::<i32>().ok()?,
        ),
        _ => return None,
    };
    Some(sign * (h * 3600 + m * 60))
}

impl PyDate {
    /// `parse_date`.
    pub(super) fn parse(value: &Value) -> Result<PyDate, String> {
        let Some(text) = value.as_str() else {
            return Err(format!(
                "Parser must be a string or character stream, not {}",
                py_type_name(value)
            ));
        };
        let naive =
            parse_date(text).ok_or_else(|| format!("Unable to parse date value: {text}"))?;
        let (_, off) = split_offset(text.trim());
        Ok(PyDate {
            naive,
            offset: off.and_then(parse_offset),
        })
    }

    /// `normalize_date`: the time zone is replaced with UTC.
    fn normalize(self) -> PyDate {
        PyDate {
            naive: self.naive,
            offset: Some(0),
        }
    }

    fn utc(naive: NaiveDateTime) -> PyDate {
        PyDate {
            naive,
            offset: Some(0),
        }
    }

    fn instant(&self) -> NaiveDateTime {
        self.naive - Duration::seconds(i64::from(self.offset.unwrap_or(0)))
    }

    fn compare(&self, other: &PyDate) -> Result<Ordering, String> {
        match (self.offset, other.offset) {
            (Some(_), Some(_)) => Ok(self.instant().cmp(&other.instant())),
            (None, None) => Ok(self.naive.cmp(&other.naive)),
            _ => Err("can't compare offset-naive and offset-aware datetimes".to_string()),
        }
    }

    fn add(self, d: Duration) -> PyDate {
        PyDate {
            naive: self.naive + d,
            offset: self.offset,
        }
    }

    /// `format_display_date`.
    pub(super) fn display(&self) -> String {
        self.naive.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    /// `format_date`.
    fn compact(&self) -> String {
        self.naive.format("%Y%m%dT%H%M%SZ").to_string()
    }

    /// The value a `datetime` parameter serializes from.
    pub(super) fn param(&self) -> Value {
        let t = self.instant().and_utc();
        let micros = t.timestamp_subsec_micros();
        if micros == 0 {
            Value::int(t.timestamp())
        } else {
            Value::Float(t.timestamp() as f64 + f64::from(micros) / 1e6)
        }
    }
}

/// An exception the traversal raises, by the Python class that matters
/// for the handlers.
#[derive(Debug)]
enum Raised {
    /// `ClientError`.
    Client(Box<crate::output::errors::ClientErrorInfo>),
    /// `DigestError` (its `str()`).
    Digest(String),
    /// Any other exception (its `str()`).
    Other(String),
    /// An error already rendered for the CLI.
    Cli(CliError),
    /// An exception of the code consuming the generator, which the
    /// traversal's handlers do not see.
    Consumer(Box<Raised>),
}

impl From<CallError> for Raised {
    fn from(e: CallError) -> Raised {
        match e {
            CallError::Service(info) => Raised::Client(info),
            other => Raised::Other(other.to_string()),
        }
    }
}

/// `S3ClientProvider`: one client per bucket region, the region found
/// with `GetBucketLocation` from a client of the command's region.
struct S3ClientProvider<'c> {
    ctx: &'c Context,
    location_region: Option<String>,
    clients: HashMap<Option<String>, S3Client<'c>>,
    regions: HashMap<String, String>,
    next_id: u32,
}

impl<'c> S3ClientProvider<'c> {
    fn client(&mut self, region: Option<String>) -> Result<&S3Client<'c>, Raised> {
        if !self.clients.contains_key(&region) {
            self.next_id += 1;
            let client = S3Client::with_config(
                self.ctx,
                ClientConfig {
                    region: region.clone().map(Some),
                    without_endpoint_url: true,
                    id: 100 + self.next_id,
                    ..Default::default()
                },
            )
            .map_err(|e| Raised::Other(e.to_string()))?;
            self.clients.insert(region.clone(), client);
        }
        Ok(&self.clients[&region])
    }

    fn get_client(&mut self, bucket: &str) -> Result<&S3Client<'c>, Raised> {
        if !self.regions.contains_key(bucket) {
            let client = self.client(self.location_region.clone())?;
            let mut p = Map::new();
            p.insert("Bucket".into(), Value::str(bucket));
            let result = client.call("GetBucketLocation", p)?;
            let region = result
                .get("LocationConstraint")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
                .unwrap_or("us-east-1")
                .to_string();
            self.regions.insert(bucket.to_string(), region);
        }
        let region = self.regions[bucket].clone();
        self.client(Some(region))
    }

    fn get_object(&mut self, bucket: &str, key: &str) -> Result<Map, Raised> {
        let client = self.get_client(bucket)?;
        let mut p = Map::new();
        p.insert("Bucket".into(), Value::str(bucket));
        p.insert("Key".into(), Value::str(key));
        Ok(client.call("GetObject", p)?)
    }
}

fn is_backfill_digest_key(key: &str) -> bool {
    key.ends_with("_backfill.json.gz")
}

/// `extract_digest_key_date` (slices of the key, by characters).
fn extract_digest_key_date(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let n = chars.len() as isize;
    let (a, b) = if is_backfill_digest_key(key) {
        (-33, -17)
    } else {
        (-24, -8)
    };
    let idx = |i: isize| (n + i).clamp(0, n) as usize;
    chars[idx(a)..idx(b).max(idx(a))].iter().collect()
}

fn key_date(key: &str) -> Result<PyDate, String> {
    PyDate::parse(&Value::str(extract_digest_key_date(key)))
}

/// The digest keys of one listing, split by type (`DigestProvider`).
#[derive(Clone)]
struct DigestKeys {
    standard: Vec<String>,
    backfill: Vec<String>,
}

/// The cache key of `load_digest_keys_in_range`: bucket, prefix, start,
/// end.
type RangeKey = (String, Option<String>, PyDate, PyDate);

struct Traverser<'c> {
    ctx: &'c Context,
    s3: S3ClientProvider<'c>,
    cloudtrail: &'c Client<'c>,
    account_id: String,
    trail_name: String,
    home_region: String,
    source_region: String,
    organization_id: Option<String>,
    cache: Vec<(RangeKey, DigestKeys)>,
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out
}

impl<'c> Traverser<'c> {
    /// `DigestProvider._create_digest_key`.
    fn digest_key(&self, start: &PyDate, prefix: Option<&str>) -> String {
        let date = start.add(Duration::minutes(-1));
        let ymd = date.naive.format("%Y/%m/%d");
        let (a, s, h, n) = (
            &self.account_id,
            &self.source_region,
            &self.home_region,
            &self.trail_name,
        );
        let date_str = date.compact();
        let mut key = match &self.organization_id {
            Some(o) => format!(
                "AWSLogs/{o}/{a}/CloudTrail-Digest/{s}/{ymd}/{a}_CloudTrail-Digest_{s}_{n}_{h}_{date_str}.json.gz"
            ),
            None => format!(
                "AWSLogs/{a}/CloudTrail-Digest/{s}/{ymd}/{a}_CloudTrail-Digest_{s}_{n}_{h}_{date_str}.json.gz"
            ),
        };
        if let Some(p) = prefix.filter(|p| !p.is_empty()) {
            key = format!("{p}/{key}");
        }
        key
    }

    /// `DigestProvider._create_digest_prefix`.
    fn digest_prefix(&self, prefix: Option<&str>) -> String {
        let mut out = String::from("AWSLogs/");
        if let Some(o) = &self.organization_id {
            out.push_str(o);
            out.push('/');
        }
        out.push_str(&format!(
            "{}/CloudTrail-Digest/{}",
            self.account_id, self.source_region
        ));
        if let Some(p) = prefix.filter(|p| !p.is_empty()) {
            out = format!("{p}/{out}");
        }
        out
    }

    /// `DigestProvider._create_digest_key_regex`.
    fn digest_key_regex(&self, prefix: Option<&str>) -> regex::Regex {
        let a = regex_escape(&self.account_id);
        let s = regex_escape(&self.source_region);
        let h = regex_escape(&self.home_region);
        let n = regex_escape(&self.trail_name);
        let mut key = match &self.organization_id {
            Some(o) => format!(
                r"AWSLogs/{o}/{a}/CloudTrail\-Digest/{s}/\d+/\d+/\d+/{a}_CloudTrail\-Digest_{s}_{n}_{h}_.+(?:_backfill)?\.json\.gz"
            ),
            None => format!(
                r"AWSLogs/{a}/CloudTrail\-Digest/{s}/\d+/\d+/\d+/{a}_CloudTrail\-Digest_{s}_{n}_{h}_.+(?:_backfill)?\.json\.gz"
            ),
        };
        if let Some(p) = prefix.filter(|p| !p.is_empty()) {
            key = format!("{}/{key}", regex_escape(p));
        }
        regex::Regex::new(&format!("^{key}$")).expect("digest key regex")
    }

    /// `DigestProvider.load_all_digest_keys_in_range`: a `ListObjects`
    /// walk from the marker that stops past the end date.
    fn load_all(
        &mut self,
        bucket: &str,
        prefix: Option<&str>,
        start: &PyDate,
        end: &PyDate,
    ) -> Result<DigestKeys, Raised> {
        let mut keys = DigestKeys {
            standard: Vec::new(),
            backfill: Vec::new(),
        };
        let mut marker = self.digest_key(start, prefix);
        let list_prefix = self.digest_prefix(prefix);
        let target_start = start.normalize().compact();
        let target_end = end.add(Duration::hours(1)).normalize().compact();
        let regex = self.digest_key_regex(prefix);
        loop {
            let client = self.s3.get_client(bucket)?;
            let mut p = Map::new();
            p.insert("Bucket".into(), Value::str(bucket));
            p.insert("Marker".into(), Value::str(&marker));
            p.insert("Prefix".into(), Value::str(&list_prefix));
            let page = {
                let _context = crate::session::useragent::ContextGuard::new(&[
                    crate::session::useragent::Feature::Paginator,
                ]);
                client.call("ListObjects", p)?
            };
            let contents = page
                .get("Contents")
                .and_then(Value::as_list)
                .cloned()
                .unwrap_or_default();
            for item in &contents {
                let Some(key) = item.get("Key").and_then(Value::as_str) else {
                    continue;
                };
                if key.is_empty() || !regex.is_match(key) {
                    continue;
                }
                let date = extract_digest_key_date(key);
                if date > target_end {
                    return Ok(keys);
                }
                if date < target_start {
                    continue;
                }
                if is_backfill_digest_key(key) {
                    keys.backfill.push(key.to_string());
                } else {
                    keys.standard.push(key.to_string());
                }
            }
            if !page.get("IsTruncated").is_some_and(Value::is_truthy) {
                return Ok(keys);
            }
            let next = page
                .get("NextMarker")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    contents
                        .last()
                        .and_then(|c| c.get("Key"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            match next {
                Some(m) if !m.is_empty() => marker = m,
                _ => return Ok(keys),
            }
        }
    }

    /// `DigestProvider.load_digest_keys_in_range` (cached per range; the
    /// returned list is the cached one the traversal pops from).
    fn load_digests(
        &mut self,
        bucket: &str,
        prefix: Option<&str>,
        start: &PyDate,
        end: &PyDate,
        backfill: bool,
    ) -> Result<usize, Raised> {
        let key = (bucket.to_string(), prefix.map(str::to_string), *start, *end);
        let index = match self.cache.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                let keys = self.load_all(bucket, prefix, start, end)?;
                self.cache.push((key, keys));
                self.cache.len() - 1
            }
        };
        let _ = backfill;
        Ok(index)
    }

    fn digests(&mut self, index: usize, backfill: bool) -> &mut Vec<String> {
        let keys = &mut self.cache[index].1;
        if backfill {
            &mut keys.backfill
        } else {
            &mut keys.standard
        }
    }

    /// `DigestTraverser._get_last_digest`.
    fn get_last_digest(
        &mut self,
        index: usize,
        backfill: bool,
        before_key: Option<&str>,
    ) -> Result<(Option<String>, Option<PyDate>), Raised> {
        let digests = self.digests(index, backfill);
        if digests.is_empty() {
            return Ok((None, None));
        }
        let Some(before_key) = before_key else {
            let next = digests.pop().expect("non-empty");
            let date = key_date(&next).map_err(Raised::Other)?.normalize();
            return Ok((Some(next), Some(date)));
        };
        let before_date = key_date(before_key).map_err(Raised::Other)?;
        while let Some(next) = self.digests(index, backfill).pop() {
            let date = key_date(&next).map_err(Raised::Other)?.normalize();
            if date.compare(&before_date).map_err(Raised::Other)? == Ordering::Less {
                return Ok((Some(next), Some(date)));
            }
        }
        Ok((None, None))
    }

    /// `PublicKeyProvider.get_public_keys` / `_load_public_keys`.
    fn load_public_keys(&self, start: &PyDate, end: &PyDate) -> Result<Map, Raised> {
        let mut p = Map::new();
        p.insert("StartTime".into(), start.param());
        p.insert("EndTime".into(), end.param());
        let result = match self
            .cloudtrail
            .try_call("ListPublicKeys", &Value::Map(p))
            .map_err(Raised::Cli)?
        {
            crate::driver::CallResult::Ok(v) => v,
            crate::driver::CallResult::ServiceError(info) => return Err(Raised::Client(info)),
        };
        let mut keys = Map::new();
        for key in result
            .get("PublicKeyList")
            .and_then(Value::as_list)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let fp = key
                .get("Fingerprint")
                .map(|f| crate::protocol::common::py_str(f).unwrap_or_default())
                .ok_or_else(|| Raised::Other("'Fingerprint'".into()))?;
            keys.insert(fp, key.clone());
        }
        if keys.is_empty() {
            return Err(Raised::Other(format!(
                "No public keys found between {} and {}",
                start.display(),
                end.display()
            )));
        }
        Ok(keys)
    }

    /// `DigestProvider.fetch_digest`.
    fn fetch_digest(&mut self, bucket: &str, key: &str) -> Result<(Value, Vec<u8>), Raised> {
        let result = self.s3.get_object(bucket, key)?;
        let invalid = || {
            Raised::Digest(format!(
                "Digest file\ts3://{bucket}/{key}\tINVALID: invalid format"
            ))
        };
        let body = match result.get("Body") {
            Some(Value::Bytes(b)) => b.clone(),
            Some(Value::Str(s)) => s.clone().into_bytes(),
            _ => Vec::new(),
        };
        let inflated = gunzip_all(&body).ok_or_else(invalid)?;
        let text = String::from_utf8(inflated.clone()).map_err(|_| invalid())?;
        let mut data = Value::parse_json(&text).map_err(|_| invalid())?;
        let metadata = result
            .get("Metadata")
            .and_then(Value::as_map)
            .cloned()
            .unwrap_or_default();
        let (Some(signature), Some(algorithm)) = (
            metadata.get("signature"),
            metadata.get("signature-algorithm"),
        ) else {
            return Err(Raised::Digest(format!(
                "Digest file\ts3://{bucket}/{key}\tINVALID: signature verification failed"
            )));
        };
        let Some(m) = data.as_map_mut() else {
            return Err(Raised::Other(format!(
                "'{}' object does not support item assignment",
                py_type_name(&data)
            )));
        };
        m.insert("_signature".into(), signature.clone());
        m.insert("_signature_algorithm".into(), algorithm.clone());
        if is_backfill_digest_key(key) {
            match metadata.get("backfill-generation-timestamp") {
                Some(t) => {
                    m.insert("_backfill_generation_timestamp".into(), t.clone());
                }
                None => return Err(invalid()),
            }
        }
        Ok((data, inflated))
    }

    /// `DigestTraverser._load_and_validate_digest`.
    fn load_and_validate(
        &mut self,
        public_keys: &mut Map,
        bucket: &str,
        key: &str,
        backfill: bool,
    ) -> Result<(Value, PyDate), Raised> {
        let (data, inflated) = self.fetch_digest(bucket, key)?;
        for required in [
            "digestPublicKeyFingerprint",
            "digestS3Bucket",
            "digestS3Object",
            "previousDigestSignature",
            "digestEndTime",
            "digestStartTime",
        ] {
            if data.get(required).is_none() {
                return Err(Raised::Digest(format!(
                    "Digest file\ts3://{bucket}/{key}\tINVALID: invalid format"
                )));
            }
        }
        if data.get("digestS3Bucket") != Some(&Value::str(bucket))
            || data.get("digestS3Object") != Some(&Value::str(key))
        {
            return Err(Raised::Digest(format!(
                "Digest file\ts3://{bucket}/{key}\tINVALID: has been moved from its original location"
            )));
        }
        let fingerprint = data
            .get("digestPublicKeyFingerprint")
            .map(|v| crate::protocol::common::py_str(v).unwrap_or_default())
            .unwrap_or_default();
        if !public_keys.contains_key(&fingerprint) && backfill {
            let stamp = PyDate::parse(data.get("_backfill_generation_timestamp").expect("set"))
                .map_err(Raised::Other)?
                .normalize();
            let more = self.load_public_keys(
                &stamp.add(Duration::hours(-1)),
                &stamp.add(Duration::hours(1)),
            )?;
            public_keys.extend(more);
        }
        let Some(public_key) = public_keys.get(&fingerprint) else {
            return Err(Raised::Digest(format!(
                "Digest file\ts3://{bucket}/{key}\tINVALID: public key not found in region {} for fingerprint {fingerprint}",
                self.home_region
            )));
        };
        let value = public_key
            .get("Value")
            .ok_or_else(|| Raised::Other("'Value'".into()))?;
        validate_signature(bucket, key, value, &data, &inflated)?;
        let end = PyDate::parse(data.get("digestEndTime").expect("required"))
            .map_err(Raised::Other)?
            .normalize();
        Ok((data, end))
    }

    /// `DigestTraverser.traverse_digests` + `_traverse_digest_chain`, with
    /// `on_digest` run where the generator yields.
    #[allow(clippy::too_many_arguments)]
    fn traverse(
        &mut self,
        start_bucket: &str,
        prefix: Option<&str>,
        start: PyDate,
        end: PyDate,
        backfill: bool,
        on_event: &mut dyn FnMut(&mut S3ClientProvider<'c>, Event<'_>) -> Result<(), Raised>,
    ) -> Result<(), Raised> {
        let start = start.normalize();
        let end = end.normalize();
        let mut bucket = start_bucket.to_string();
        let mut index = self.load_digests(&bucket, prefix, &start, &end, backfill)?;
        let mut public_keys = if backfill {
            Map::new()
        } else {
            self.load_public_keys(&start, &end.add(Duration::hours(2)))?
        };
        let (mut key, mut end_date) = self.get_last_digest(index, backfill, None)?;
        let mut last_start = end_date;
        while let Some(k) = key.clone() {
            let Some(ls) = last_start else { break };
            if start.compare(&ls).map_err(Raised::Other)? == Ordering::Greater {
                break;
            }
            let step: Result<(), Raised> = (|| {
                let (digest, digest_end) =
                    self.load_and_validate(&mut public_keys, &bucket, &k, backfill)?;
                end_date = Some(digest_end);
                last_start = Some(
                    PyDate::parse(digest.get("digestStartTime").expect("required"))
                        .map_err(Raised::Other)?
                        .normalize(),
                );
                let previous_bucket = digest
                    .get("previousDigestS3Bucket")
                    .filter(|v| !matches!(v, Value::Null))
                    .cloned();
                let previous_key = digest
                    .get("previousDigestS3Object")
                    .filter(|v| !matches!(v, Value::Null))
                    .cloned();
                on_event(&mut self.s3, Event::Digest(&digest))
                    .map_err(|e| Raised::Consumer(Box::new(e)))?;
                match (previous_bucket, previous_key) {
                    (Some(pb), Some(pk)) => {
                        key = Some(crate::protocol::common::py_str(&pk).unwrap_or_default());
                        let pb = crate::protocol::common::py_str(&pb).unwrap_or_default();
                        if pb != bucket {
                            bucket = pb;
                            index = self.load_digests(
                                &bucket,
                                prefix,
                                &start,
                                end_date.as_ref().expect("set"),
                                backfill,
                            )?;
                        }
                    }
                    _ => {
                        let (next_key, next_end) =
                            self.get_last_digest(index, backfill, Some(&k))?;
                        if next_key.is_some() {
                            on_event(
                                &mut self.s3,
                                Event::Gap {
                                    next_end_date: next_end.expect("with key"),
                                    last_start_date: last_start.expect("set"),
                                },
                            )?;
                        }
                        key = next_key;
                        end_date = next_end;
                    }
                }
                Ok(())
            })();
            let message = match step {
                Ok(()) => continue,
                Err(Raised::Client(info)) if info.code == "NoSuchKey" => Event::Missing {
                    bucket: &bucket,
                    key: &k,
                },
                Err(Raised::Client(info)) => return Err(Raised::Client(info)),
                Err(Raised::Cli(e)) => return Err(Raised::Cli(e)),
                Err(Raised::Consumer(e)) => return Err(*e),
                Err(Raised::Digest(m)) => {
                    let (next_key, next_end) = self.get_last_digest(index, backfill, Some(&k))?;
                    on_event(&mut self.s3, Event::Invalid(&m))?;
                    key = next_key;
                    end_date = next_end;
                    continue;
                }
                Err(Raised::Other(m)) => {
                    let (next_key, next_end) = self.get_last_digest(index, backfill, Some(&k))?;
                    on_event(
                        &mut self.s3,
                        Event::Invalid(&format!("Digest file\ts3://{bucket}/{k}\tINVALID: {m}")),
                    )?;
                    key = next_key;
                    end_date = next_end;
                    continue;
                }
            };
            let (next_key, next_end) = self.get_last_digest(index, backfill, Some(&k))?;
            on_event(&mut self.s3, message)?;
            key = next_key;
            end_date = next_end;
        }
        let _ = end_date;
        Ok(())
    }
}

/// What the traversal reports to the command.
enum Event<'a> {
    /// A validated digest (the generator's `yield`).
    Digest(&'a Value),
    /// `on_gap`.
    Gap {
        next_end_date: PyDate,
        last_start_date: PyDate,
    },
    /// `on_missing`.
    Missing { bucket: &'a str, key: &'a str },
    /// `on_invalid` with the message.
    Invalid(&'a str),
}

/// `zlib.decompress(data, MAX_WBITS | 16)`: the first gzip member, `None`
/// when it is not one or is truncated.
fn gunzip_all(data: &[u8]) -> Option<Vec<u8>> {
    let mut d = flate2::Decompress::new_gzip(15);
    let mut out = Vec::new();
    loop {
        let consumed = d.total_in() as usize;
        out.reserve(64 * 1024);
        let status = d
            .decompress_vec(&data[consumed..], &mut out, flate2::FlushDecompress::Finish)
            .ok()?;
        match status {
            flate2::Status::StreamEnd => return Some(out),
            flate2::Status::Ok => {}
            flate2::Status::BufError => {
                if d.total_in() as usize >= data.len() && out.len() < out.capacity() {
                    return None;
                }
            }
        }
    }
}

/// `zlib.decompressobj(MAX_WBITS | 16)` over a whole body: the inflated
/// data, whether bytes follow the end of the stream (`unused_data`), or
/// `None` on a decompression error.
fn gunzip_stream(data: &[u8]) -> Option<(Vec<u8>, bool)> {
    let mut d = flate2::Decompress::new_gzip(15);
    let mut out = Vec::new();
    loop {
        let consumed = d.total_in() as usize;
        out.reserve(64 * 1024);
        let before = (d.total_in(), d.total_out());
        let status = d
            .decompress_vec(&data[consumed..], &mut out, flate2::FlushDecompress::None)
            .ok()?;
        match status {
            flate2::Status::StreamEnd => {
                return Some((out, (d.total_in() as usize) < data.len()));
            }
            _ => {
                if (d.total_in(), d.total_out()) == before {
                    return Some((out, false));
                }
            }
        }
    }
}

/// A PKCS #1 `RSAPublicKey` DER: the modulus and exponent.
pub(super) fn parse_rsa_public_key(der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    fn read(data: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
        if data.first() != Some(&tag) {
            return None;
        }
        let first = *data.get(1)?;
        let (len, header) = if first < 0x80 {
            (first as usize, 2)
        } else {
            let n = (first & 0x7f) as usize;
            if n == 0 || n > 4 {
                return None;
            }
            let mut len = 0usize;
            for b in data.get(2..2 + n)? {
                len = (len << 8) | *b as usize;
            }
            (len, 2 + n)
        };
        let body = data.get(header..header + len)?;
        Some((body, &data[header + len..]))
    }
    let (seq, rest) = read(der, 0x30)?;
    if !rest.is_empty() {
        return None;
    }
    let (n, rest) = read(seq, 0x02)?;
    let (e, rest) = read(rest, 0x02)?;
    if !rest.is_empty() || n.is_empty() || e.is_empty() {
        return None;
    }
    let strip = |b: &[u8]| {
        let i = b.iter().position(|x| *x != 0).unwrap_or(b.len());
        b[i..].to_vec()
    };
    Some((strip(n), strip(e)))
}

/// SHA256withRSA (PKCS #1 v1.5) verification of `message`.
pub(super) fn rsa_verify(n: &[u8], e: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let key = ring::signature::RsaPublicKeyComponents { n, e };
    key.verify(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        message,
        signature,
    )
    .is_ok()
}

/// `base64.b64decode` (non-alphabet characters discarded, padding
/// checked); the message of `binascii.Error` otherwise.
pub(super) fn py_b64decode(text: &str) -> Result<Vec<u8>, String> {
    crate::protocol::common::b64decode(text).map_err(|e| e.to_string())
}

/// `binascii.unhexlify`.
pub(super) fn unhexlify(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 == 1 {
        return Err("Odd-length string".to_string());
    }
    hex::decode(text).map_err(|_| "Non-hexadecimal digit found".to_string())
}

/// `Sha256RSADigestValidator.validate`.
fn validate_signature(
    bucket: &str,
    key: &str,
    public_key: &Value,
    data: &Value,
    inflated: &[u8],
) -> Result<(), Raised> {
    let text = public_key.as_str().ok_or_else(|| {
        Raised::Other("argument should be a bytes-like object or ASCII string, not 'int'".into())
    })?;
    let der = py_b64decode(text).map_err(Raised::Other)?;
    let Some((n, e)) = parse_rsa_public_key(&der) else {
        return Err(Raised::Digest(format!(
            "Digest file\ts3://{bucket}/{key}\tINVALID: Unable to load PKCS #1 key with fingerprint {}",
            data.get("digestPublicKeyFingerprint")
                .map(|v| crate::protocol::common::py_str(v).unwrap_or_default())
                .unwrap_or_default()
        )));
    };
    let field = |k: &str| {
        data.get(k)
            .map(|v| crate::protocol::common::py_str(v).unwrap_or_default())
            .unwrap_or_default()
    };
    let previous = match data.get("previousDigestSignature") {
        Some(Value::Null) | None => "null".to_string(),
        Some(v) => crate::protocol::common::py_str(v).unwrap_or_default(),
    };
    let to_sign = format!(
        "{}\n{}/{}\n{}\n{previous}",
        field("digestEndTime"),
        field("digestS3Bucket"),
        field("digestS3Object"),
        hex::encode(Sha256::digest(inflated)),
    );
    let signature = unhexlify(&field("_signature")).map_err(Raised::Other)?;
    if !rsa_verify(&n, &e, to_sign.as_bytes(), &signature) {
        return Err(Raised::Digest(format!(
            "Digest file\ts3://{bucket}/{key}\tINVALID: signature verification failed"
        )));
    }
    Ok(())
}

/// The counters and output of `CloudTrailValidateLogs`.
struct Report {
    verbose: bool,
    valid_digests: usize,
    invalid_digests: usize,
    valid_backfill: usize,
    invalid_backfill: usize,
    valid_logs: usize,
    invalid_logs: usize,
    last_double_space: bool,
    found_start: Option<PyDate>,
    found_end: Option<PyDate>,
    start: PyDate,
    end: PyDate,
}

fn out(text: &str) {
    crate::output::stdout::write(text.as_bytes());
}

fn err(text: &str) {
    use std::io::Write as _;
    let mut e = std::io::stderr().lock();
    let _ = e.write_all(text.as_bytes());
}

impl Report {
    /// `_write_status`.
    fn status(&mut self, message: &str, is_error: bool) {
        if is_error {
            if self.last_double_space {
                err(&format!("{message}\n\n"));
            } else {
                err(&format!("\n{message}\n\n"));
            }
            self.last_double_space = true;
        } else if self.verbose {
            self.last_double_space = false;
            out(&format!("{message}\n"));
        }
    }

    /// `_track_found_times`.
    fn track(&mut self, digest: &Value) -> Result<(), Raised> {
        let start = PyDate::parse(digest.get("digestStartTime").expect("required"))
            .map_err(Raised::Other)?;
        let earliest = match start.compare(&self.start).map_err(Raised::Other)? {
            Ordering::Less => self.start,
            _ => start,
        };
        let replace = match &self.found_start {
            None => true,
            Some(f) => earliest.compare(f).map_err(Raised::Other)? == Ordering::Less,
        };
        if replace {
            self.found_start = Some(earliest);
        }
        let end =
            PyDate::parse(digest.get("digestEndTime").expect("required")).map_err(Raised::Other)?;
        let latest = match end.compare(&self.end).map_err(Raised::Other)? {
            Ordering::Greater => self.end,
            _ => end,
        };
        let replace = match &self.found_end {
            None => true,
            Some(f) => latest.compare(f).map_err(Raised::Other)? == Ordering::Greater,
        };
        if replace {
            self.found_end = Some(latest);
        }
        Ok(())
    }

    fn ratio(valid: usize, invalid: usize, name: &str) {
        let total = valid + invalid;
        if total > 0 {
            out(&format!("\n{valid}/{total} {name} files valid"));
            if invalid > 0 {
                out(&format!(", {invalid}/{total} {name} files INVALID"));
            }
        }
    }

    /// `_write_summary_text`.
    fn summary(&self) {
        if !self.last_double_space {
            out("\n");
        }
        out(&format!(
            "Results requested for {} to {}\n",
            self.start.display(),
            self.end.display()
        ));
        let valid = self.valid_digests + self.valid_backfill;
        let invalid = self.invalid_digests + self.invalid_backfill;
        if valid == 0 && invalid == 0 {
            out("No digests found\n");
            return;
        }
        match (&self.found_start, &self.found_end) {
            (Some(s), Some(e)) => out(&format!(
                "Results found for {} to {}:\n",
                s.display(),
                e.display()
            )),
            _ => out("No valid digests found in range\n"),
        }
        Report::ratio(self.valid_digests, self.invalid_digests, "digest");
        Report::ratio(
            self.valid_backfill,
            self.invalid_backfill,
            "backfill digest",
        );
        Report::ratio(self.valid_logs, self.invalid_logs, "log");
        out("\n");
    }
}

/// `CloudTrailValidateLogs._download_log`.
fn download_log(
    s3: &mut S3ClientProvider<'_>,
    report: &mut Report,
    log: &Value,
) -> Result<(), Raised> {
    let field = |k: &str| -> Result<String, Raised> {
        log.get(k)
            .map(|v| crate::protocol::common::py_str(v).unwrap_or_default())
            .ok_or_else(|| Raised::Other(format!("'{k}'")))
    };
    let bucket = field("s3Bucket")?;
    let key = field("s3Object")?;
    let result = (|| -> Result<Option<&'static str>, Raised> {
        let response = s3.get_object(&bucket, &key)?;
        let body = match response.get("Body") {
            Some(Value::Bytes(b)) => b.clone(),
            Some(Value::Str(s)) => s.clone().into_bytes(),
            _ => Vec::new(),
        };
        let (inflated, trailing) =
            gunzip_stream(&body).ok_or_else(|| Raised::Other("invalid".into()))?;
        if trailing {
            return Ok(Some("unexpected data after end of compressed stream"));
        }
        let hash = hex::encode(Sha256::digest(&inflated));
        if Value::str(&hash)
            != *log
                .get("hashValue")
                .ok_or_else(|| Raised::Other("'hashValue'".into()))?
        {
            return Ok(Some("hash value doesn't match"));
        }
        Ok(None)
    })();
    match result {
        Ok(None) => {
            report.valid_logs += 1;
            report.status(&format!("Log file\ts3://{bucket}/{key}\tvalid"), false);
        }
        Ok(Some(reason)) => {
            report.invalid_logs += 1;
            report.status(
                &format!("Log file\ts3://{bucket}/{key}\tINVALID: {reason}"),
                true,
            );
        }
        Err(Raised::Client(info)) if info.code == "NoSuchKey" => {
            report.invalid_logs += 1;
            report.status(
                &format!("Log file\ts3://{bucket}/{key}\tINVALID: not found"),
                true,
            );
        }
        Err(e @ (Raised::Client(_) | Raised::Cli(_) | Raised::Consumer(_))) => return Err(e),
        Err(_) => {
            report.invalid_logs += 1;
            report.status(
                &format!("Log file\ts3://{bucket}/{key}\tINVALID: invalid format"),
                true,
            );
        }
    }
    Ok(())
}

fn raised_error(ctx: &Context, e: Raised) -> CliError {
    match e {
        Raised::Client(info) => client_error(ctx, &info),
        Raised::Cli(e) => e,
        Raised::Digest(m) | Raised::Other(m) => general(ctx, &m),
        Raised::Consumer(e) => raised_error(ctx, *e),
    }
}

/// `CloudTrailValidateLogs._run_main`.
fn validate_logs(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = [
        ArgDef::new("trail-arn").type_name("string").required(),
        ArgDef::new("start-time").type_name("string").required(),
        ArgDef::new("end-time").type_name("string"),
        ArgDef::new("s3-bucket").type_name("string"),
        ArgDef::new("s3-prefix").type_name("string"),
        ArgDef::new("account-id").type_name("string"),
        ArgDef::new("verbose").type_name("boolean").flag(),
    ];
    let Some(a) = basic::parse(ctx, &["cloudtrail", "validate-logs"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let start_raw = a.str("start-time").unwrap_or_default();
    let start = parse_date(&start_raw)
        .ok_or_else(|| general(ctx, &format!("Unable to parse date value: {start_raw}")))?;
    let end = match a.str("end-time").filter(|s| !s.is_empty()) {
        Some(e) => parse_date(&e)
            .ok_or_else(|| general(ctx, &format!("Unable to parse date value: {e}")))?,
        None => Utc::now().naive_utc(),
    };
    if start > end {
        return Err(general(
            ctx,
            "Invalid time range specified: start-time must occur before end-time",
        ));
    }
    let (start, end) = (PyDate::utc(start), PyDate::utc(end));
    // `setup_services`.
    let region = ctx.globals.region.clone();
    let organizations = Client::with_config(
        ctx,
        "organizations",
        ClientConfig {
            region: region.clone().map(Some),
            without_endpoint_url: true,
            id: 0,
            ..Default::default()
        },
    )?;
    let cloudtrail = Client::new(ctx, "cloudtrail")?;
    // `create_digest_traverser`.
    let arn = a.str("trail-arn").unwrap_or_default();
    let re = regex::Regex::new(r"^arn:.+:cloudtrail:.+:\d{12}:trail/.+").expect("regex");
    if !re.is_match(&arn) {
        return Err(general(ctx, &format!("Invalid trail ARN provided: {arn}")));
    }
    let mut account_id = a.str("account-id");
    let mut organization_id = None;
    let (bucket, prefix) = match a.str("s3-bucket") {
        Some(b) => (b, a.str("s3-prefix")),
        None => {
            let trails = cloudtrail.call("DescribeTrails", &Value::map())?;
            let trail = trails
                .get("trailList")
                .and_then(Value::as_list)
                .and_then(|l| {
                    l.iter()
                        .find(|t| t.get("TrailARN").and_then(Value::as_str) == Some(arn.as_str()))
                })
                .cloned()
                .ok_or_else(|| general(ctx, &format!("A trail could not be found for {arn}")))?;
            let bucket = trail
                .get("S3BucketName")
                .map(|b| crate::protocol::common::py_str(b).unwrap_or_default())
                .ok_or_else(|| general(ctx, "'S3BucketName'"))?;
            let prefix = trail
                .get("S3KeyPrefix")
                .filter(|p| !matches!(p, Value::Null))
                .map(|p| crate::protocol::common::py_str(p).unwrap_or_default());
            if trail
                .get("IsOrganizationTrail")
                .is_some_and(Value::is_truthy)
            {
                if !account_id.as_deref().is_some_and(|a| !a.is_empty()) {
                    return Err(general(
                        ctx,
                        "Missing required parameter for organization trail: '--account-id'",
                    ));
                }
                let org = organizations.call("DescribeOrganization", &Value::map())?;
                organization_id = Some(
                    org.get("Organization")
                        .and_then(|o| o.get("Id"))
                        .map(|i| crate::protocol::common::py_str(i).unwrap_or_default())
                        .ok_or_else(|| general(ctx, "'Organization'"))?,
                );
            }
            (bucket, prefix)
        }
    };
    let parts: Vec<&str> = arn.split(':').collect();
    let home_region = parts[3].to_string();
    let trail_name = arn.rsplit('/').next().unwrap_or_default().to_string();
    if !account_id.as_deref().is_some_and(|a| !a.is_empty()) {
        account_id = Some(parts[4].to_string());
    }
    let mut traverser = Traverser {
        ctx,
        s3: S3ClientProvider {
            ctx,
            location_region: region.clone(),
            clients: HashMap::new(),
            regions: HashMap::new(),
            next_id: 0,
        },
        cloudtrail: &cloudtrail,
        account_id: account_id.unwrap_or_default(),
        trail_name,
        source_region: region.clone().unwrap_or_else(|| home_region.clone()),
        home_region,
        organization_id,
        cache: Vec::new(),
    };
    let _ = traverser.ctx;
    out(&format!(
        "Validating log files for trail {arn} between {} and {}\n\n",
        start.display(),
        end.display()
    ));
    let mut report = Report {
        verbose: a.get("verbose").is_some_and(Value::is_truthy),
        valid_digests: 0,
        invalid_digests: 0,
        valid_backfill: 0,
        invalid_backfill: 0,
        valid_logs: 0,
        invalid_logs: 0,
        last_double_space: true,
        found_start: None,
        found_end: None,
        start,
        end,
    };
    for backfill in [false, true] {
        let tag = if backfill { "(backfill) " } else { "" };
        let result = traverser.traverse(
            &bucket,
            prefix.as_deref(),
            start,
            end,
            backfill,
            &mut |s3, event| {
                match event {
                    Event::Digest(digest) => {
                        report.track(digest)?;
                        if backfill {
                            report.valid_backfill += 1;
                        } else {
                            report.valid_digests += 1;
                        }
                        report.status(
                            &format!(
                                "{tag}Digest file\ts3://{}/{}\tvalid",
                                digest
                                    .get("digestS3Bucket")
                                    .map(|v| crate::protocol::common::py_str(v).unwrap_or_default())
                                    .unwrap_or_default(),
                                digest
                                    .get("digestS3Object")
                                    .map(|v| crate::protocol::common::py_str(v).unwrap_or_default())
                                    .unwrap_or_default()
                            ),
                            false,
                        );
                        let logs = match digest.get("logFiles") {
                            Some(Value::List(l)) => l.clone(),
                            Some(v) if !v.is_truthy() => Vec::new(),
                            Some(_) => return Err(Raised::Other("logFiles".into())),
                            None => return Err(Raised::Other("'logFiles'".into())),
                        };
                        for log in &logs {
                            download_log(s3, &mut report, log)?;
                        }
                    }
                    Event::Gap {
                        next_end_date,
                        last_start_date,
                    } => report.status(
                        &format!(
                            "{tag}No log files were delivered by CloudTrail between {} and {}",
                            next_end_date.display(),
                            last_start_date.display()
                        ),
                        true,
                    ),
                    Event::Missing { bucket, key } => {
                        if backfill {
                            report.invalid_backfill += 1;
                        } else {
                            report.invalid_digests += 1;
                        }
                        report.status(
                            &format!("{tag}Digest file\ts3://{bucket}/{key}\tINVALID: not found"),
                            true,
                        );
                    }
                    Event::Invalid(message) => {
                        if backfill {
                            report.invalid_backfill += 1;
                        } else {
                            report.invalid_digests += 1;
                        }
                        report.status(&format!("{tag}{message}"), true);
                    }
                }
                Ok(())
            },
        );
        if let Err(e) = result {
            return Err(raised_error(ctx, e));
        }
    }
    report.summary();
    if report.invalid_digests + report.invalid_backfill > 0 || report.invalid_logs > 0 {
        return Ok(ExitCode::S3TransferFailed);
    }
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        let d = |s| parse_date(s).map(|d| d.to_string());
        assert_eq!(
            d("2015-01-08T05:21:42Z").as_deref(),
            Some("2015-01-08 05:21:42")
        );
        assert_eq!(d("2015-01-08").as_deref(), Some("2015-01-08 00:00:00"));
        assert_eq!(
            d("2015-01-08T05:21:42+09:00").as_deref(),
            Some("2015-01-08 05:21:42")
        );
        assert_eq!(d("nope"), None);
    }
}
