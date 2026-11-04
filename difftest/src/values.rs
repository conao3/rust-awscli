//! Seeded value generation from botocore shapes.
//!
//! [`Generator`] walks a shape and produces a [`G`] tree that respects the
//! shape's type, enum, `min` / `max`, `required`, and union constraints,
//! with recursion cut off at a depth limit. Leaf values are drawn from
//! pools that deliberately contain edge cases: number lexemes such as
//! `1.50`, `1e2`, `-0.0`, integers beyond 2^53, timestamps rendered in
//! several textual forms, East-Asian wide and ambiguous-width characters,
//! empty strings, empty lists and maps, and arbitrary blobs.

use awsr::bjson::Node;
use awsr::model::Shape;
use serde_json::{Map as JsonMap, Value as Json};

use crate::rng::Rng;

/// A generated value. Numbers keep the lexeme they are sent with.
#[derive(Debug, Clone, PartialEq)]
pub enum G {
    Null,
    Bool(bool),
    Int(String),
    Float(FloatLit),
    Str(String),
    Blob(Vec<u8>),
    Ts(Ts),
    List(Vec<G>),
    Map(Vec<(String, G)>),
    /// Structure members by member name, in generation order.
    Struct(Vec<(String, G)>),
    Doc(Json),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FloatLit {
    pub value: f64,
    /// The text used on JSON / XML / header wires.
    pub text: String,
}

/// A timestamp: seconds and microseconds since the epoch, plus a variant
/// number choosing the textual form at serialization time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ts {
    pub secs: i64,
    pub micros: u32,
    pub form: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Json,
    RestJson,
    RestXml,
    Cbor,
    Query,
    Ec2,
}

impl Proto {
    pub fn parse(s: &str) -> Option<Proto> {
        Some(match s {
            "json" => Proto::Json,
            "rest-json" => Proto::RestJson,
            "rest-xml" => Proto::RestXml,
            "smithy-rpc-v2-cbor" => Proto::Cbor,
            "query" => Proto::Query,
            "ec2" => Proto::Ec2,
            _ => return None,
        })
    }
}

/// Where a string ends up, which limits the characters it may contain.
#[derive(Debug, Clone, Copy, Default)]
struct Ctx {
    /// An HTTP header value (ASCII, no surrounding spaces).
    header: bool,
    /// An element of a header list (also no commas).
    no_comma: bool,
    /// A header name suffix of a `headers` map.
    header_key: bool,
    /// A host label or endpoint context parameter.
    label: bool,
    /// A URI path label.
    uri: bool,
    bucket: bool,
    account: bool,
    /// An input member named `*Arn`, which handlers and endpoint rules
    /// may parse.
    arn: bool,
}

/// Response header names the stub server owns or that would change how
/// the client reads the body.
const RESERVED_HEADERS: &[&str] = &[
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "date",
    "x-amz-request-id",
    "x-amz-id-2",
    "x-amzn-requestid",
    "x-amz-crc32",
    "content-md5",
];

pub fn reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    RESERVED_HEADERS.contains(&lower.as_str()) || lower.starts_with("x-amz-checksum-")
}

const HARD_DEPTH: usize = 40;

pub struct Generator<'r> {
    pub rng: &'r mut Rng,
    pub dir: Dir,
    pub proto: Proto,
    pub max_depth: usize,
}

const PLAIN_STRINGS: &[&str] = &[
    "",
    "simple",
    "Hello, World",
    "日本語のテキスト",
    "ＦＵＬＬＷＩＤＴＨ　ｓｐａｃｅ",
    "±×÷§°¶ ΑΩαω ДЖЯ",
    "─│┌┐└┘",
    "①②③ ※ ○●",
    "😀 emoji",
    "e\u{301}clair",
    "line1\nline2",
    "tab\tseparated",
    "  padded  ",
    "<xml & \"quotes\" 'apos'>",
    "None",
    "true",
    "123",
    "1.50",
    "null",
    "a|b|c",
    "-dash",
    "混在 mixed ±",
    "a,b",
    "http://example.com/path?q=1&r=2",
    "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore",
];

const HEADER_STRINGS: &[&str] = &[
    "simple",
    "Hello World",
    "value-with-dashes",
    "a=b;c=d",
    "/path/to/thing",
    "123",
    "true",
    "\"etag-like\"",
    "text/plain",
];

const URI_STRINGS: &[&str] = &[
    "simple",
    "with space",
    "ünïcödé",
    "a+b",
    "x%y",
    "日本",
    "dots.and_underscores~",
    "a/b",
];

const DATE_SECS: &[i64] = &[
    0,
    1,
    86_399,
    951_782_400,
    1_422_172_800,
    1_700_000_000,
    2_147_483_647,
    4_102_444_799,
];

impl Generator<'_> {
    pub fn structure(&mut self, shape: &Shape<'_>, depth: usize, skip: &[&str]) -> G {
        let required = shape.required();
        let members: Vec<_> = shape
            .members()
            .into_iter()
            .filter(|(name, m)| !skip.contains(name) && self.eligible(m))
            .collect();
        // Past `max_depth` only required members and one union member are
        // generated; the hard cap only guards against required cycles.
        if depth > HARD_DEPTH {
            return G::Struct(vec![]);
        }
        if shape.is_union() {
            if members.is_empty() {
                return G::Struct(vec![]);
            }
            let scalars: Vec<_> = members
                .iter()
                .filter(|(_, m)| !matches!(m.type_name(), "structure" | "list" | "map"))
                .collect();
            let (name, m) = if depth >= self.max_depth && !scalars.is_empty() {
                *scalars[self.rng.below(scalars.len() as u64) as usize]
            } else {
                members[self.rng.below(members.len() as u64) as usize]
            };
            let (name, m) = (&name, &m);
            let v = self.member(name, m, depth + 1);
            return G::Struct(vec![(name.to_string(), v)]);
        }
        let mut out = vec![];
        for (name, m) in &members {
            // S3 handlers (`decode_list_object*`) index these unconditionally.
            let s3_key = self.dir == Dir::Output
                && shape.model.name == "s3"
                && matches!(*name, "Key" | "Prefix");
            let req = required.contains(name) || s3_key;
            let p = if depth >= self.max_depth { 0 } else { 3 };
            if req || self.rng.chance(p, 5) {
                let v = self.member(name, m, depth + 1);
                out.push((name.to_string(), v));
            }
        }
        G::Struct(out)
    }

    fn eligible(&mut self, m: &Shape<'_>) -> bool {
        if m.is_event_stream() || m.get_bool("eventheader") || m.get_bool("eventpayload") {
            return false;
        }
        match self.dir {
            Dir::Input => {
                let content_length = m.member_get("location").and_then(Node::as_str)
                    == Some("header")
                    && m.get_str("locationName")
                        .is_some_and(|n| n.eq_ignore_ascii_case("content-length"));
                !m.get_bool("idempotencyToken") && !m.is_streaming() && !content_length
            }
            Dir::Output => match m.member_get("location").and_then(Node::as_str) {
                Some("header") => {
                    let name = m.get_str("locationName").unwrap_or_default();
                    if reserved_header(name) {
                        return false;
                    }
                    if m.type_name() == "list" {
                        let t = m.list_member().map(|s| s.type_name()).unwrap_or_default();
                        return t != "timestamp" && t != "structure";
                    }
                    true
                }
                _ => true,
            },
        }
    }

    pub fn member(&mut self, name: &str, m: &Shape<'_>, depth: usize) -> G {
        let location = m.member_get("location").and_then(Node::as_str);
        let mut ctx = Ctx {
            header: matches!(location, Some("header") | Some("headers")),
            ..Ctx::default()
        };
        if self.dir == Dir::Input {
            ctx.label = m.get_bool("hostLabel") || m.member_get("contextParam").is_some();
            ctx.uri = location == Some("uri");
            ctx.bucket = name == "Bucket";
            ctx.account = name == "AccountId";
            ctx.arn = name.ends_with("Arn") || name.ends_with("ARN");
        }
        if self.dir == Dir::Output && location == Some("statusCode") {
            return G::Int(self.rng.pick(&["200", "201", "202"]).to_string());
        }
        if location == Some("headers") {
            return self.header_map(m);
        }
        self.value(m, depth, ctx)
    }

    fn header_map(&mut self, m: &Shape<'_>) -> G {
        let n = self.rng.below(3);
        let mut out: Vec<(String, G)> = vec![];
        for i in 0..n {
            let key = format!("{}{}", self.rng.pick(&["key", "meta", "x", "name"]), i);
            let v = self.header_string(0, 40);
            if let Some(vs) = m.map_value()
                && vs.type_name() == "string"
            {
                out.push((key, G::Str(v)));
            }
        }
        G::Map(out)
    }

    /// A value of any shape outside a member context.
    pub fn any(&mut self, s: &Shape<'_>, depth: usize) -> G {
        self.value(s, depth, Ctx::default())
    }

    fn value(&mut self, s: &Shape<'_>, depth: usize, ctx: Ctx) -> G {
        match type_of(s) {
            "structure" if s.is_document() => G::Doc(self.doc(0)),
            "structure" => self.structure(s, depth, &[]),
            "list" => {
                let Some(member) = s.list_member() else {
                    return G::List(vec![]);
                };
                let n = self.count(s, depth);
                let elem_ctx = Ctx {
                    no_comma: ctx.header,
                    ..ctx
                };
                let mut items = vec![];
                for _ in 0..n {
                    items.push(self.value(&member, depth + 1, elem_ctx));
                }
                G::List(items)
            }
            "map" => {
                let (Some(k), Some(v)) = (s.map_key(), s.map_value()) else {
                    return G::Map(vec![]);
                };
                let n = self.count(s, depth);
                let mut entries: Vec<(String, G)> = vec![];
                let mut tries = 0;
                while (entries.len() as u64) < n && tries < 20 {
                    tries += 1;
                    let key = match self.value(&k, depth + 1, Ctx::default()) {
                        G::Str(s) => s,
                        _ => continue,
                    };
                    if entries.iter().any(|(e, _)| *e == key) {
                        continue;
                    }
                    let val = self.value(&v, depth + 1, ctx);
                    entries.push((key, val));
                }
                G::Map(entries)
            }
            // botocore `is_json_value_header`: only header members carry
            // decoded JSON; a body `jsonvalue` string is plain text.
            "string" if s.get_bool("jsonvalue") && ctx.header => G::Doc(self.doc(1)),
            "string" if s.get_bool("jsonvalue") => {
                G::Str(serde_json::to_string(&self.doc(1)).unwrap_or_default())
            }
            "string" => {
                let mut enums = s.enum_values();
                if self.dir == Dir::Input {
                    // botocore computes only CRC and SHA request checksums.
                    enums.retain(|e| *e != "MD5");
                }
                if !enums.is_empty() {
                    return G::Str(self.rng.pick(&enums).to_string());
                }
                G::Str(self.string(s, ctx))
            }
            "integer" | "long" | "short" | "byte" => G::Int(self.int(s)),
            "float" | "double" => G::Float(self.float(s, ctx)),
            "boolean" => G::Bool(self.rng.chance(1, 2)),
            "blob" => {
                let (min, max) = bounds(s);
                // The CLI decodes an empty base64 input blob to None.
                let floor = usize::from(self.dir == Dir::Input);
                let lo = (min.unwrap_or(0).max(0) as usize).max(floor);
                let hi = max.map(|m| m as usize).unwrap_or(24).max(lo);
                let n = self.rng.range(lo as i64, hi.min(lo + 24) as i64) as usize;
                if self.rng.chance(1, 3) {
                    let text: String = (0..n)
                        .map(|_| char::from(b'a' + self.rng.below(26) as u8))
                        .collect();
                    G::Blob(text.into_bytes())
                } else {
                    G::Blob(self.rng.bytes(n))
                }
            }
            "timestamp" => G::Ts(self.timestamp()),
            _ => G::Null,
        }
    }

    fn count(&mut self, s: &Shape<'_>, depth: usize) -> u64 {
        let (min, max) = bounds(s);
        let lo = min.unwrap_or(0).max(0) as u64;
        if depth >= self.max_depth {
            return lo;
        }
        let hi = max
            .map(|m| m.max(0) as u64)
            .unwrap_or(3)
            .min(lo + 3)
            .max(lo);
        lo + self.rng.below(hi - lo + 1)
    }

    fn string(&mut self, s: &Shape<'_>, ctx: Ctx) -> String {
        let (min, max) = bounds(s);
        let min = min.unwrap_or(0).max(0) as usize;
        let max = max.map(|m| m.max(0) as usize).unwrap_or(usize::MAX);
        if ctx.account {
            return (0..12)
                .map(|_| char::from(b'0' + self.rng.below(10) as u8))
                .collect();
        }
        if ctx.arn {
            let name = self.alnum_lower(8);
            let arn = if s.model.name == "cloudfront-keyvaluestore" {
                format!("arn:aws:cloudfront::123456789012:key-value-store/{name}")
            } else {
                format!(
                    "arn:aws:{}:us-east-1:123456789012:resource/{name}",
                    s.model.endpoint_prefix()
                )
            };
            if (min..=max).contains(&arn.len()) {
                return arn;
            }
        }
        if ctx.bucket {
            let n = self.rng.range(3, 16) as usize;
            return self.alnum_lower(n.clamp(min.max(3), max.max(3)));
        }
        if ctx.label {
            let n = self.rng.range(3, 12) as usize;
            return self.alnum_lower(n.clamp(min.max(1), max.max(1)));
        }
        if ctx.header_key {
            return self.alnum_lower(6);
        }
        if ctx.header {
            let mut v = self.header_string(min, max);
            if ctx.no_comma {
                v = v.replace(',', ";");
            }
            return v;
        }
        let candidate = if ctx.uri {
            self.rng.pick(URI_STRINGS).to_string()
        } else if self.rng.chance(1, 4) {
            let n = self.rng.range(1, 12) as usize;
            self.alnum(n)
        } else {
            self.rng.pick(PLAIN_STRINGS).to_string()
        };
        fit_len(candidate, min.max(usize::from(ctx.uri)), max, self)
    }

    fn header_string(&mut self, min: usize, max: usize) -> String {
        let candidate = if self.rng.chance(1, 3) {
            let n = self.rng.range(1, 16) as usize;
            self.alnum(n)
        } else {
            self.rng.pick(HEADER_STRINGS).to_string()
        };
        fit_len(candidate, min.max(1), max, self)
    }

    pub fn alnum(&mut self, n: usize) -> String {
        const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        (0..n).map(|_| char::from(*self.rng.pick(CHARS))).collect()
    }

    fn alnum_lower(&mut self, n: usize) -> String {
        const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut s: String = (0..n).map(|_| char::from(*self.rng.pick(CHARS))).collect();
        s.replace_range(0..1, "a");
        s
    }

    fn int(&mut self, s: &Shape<'_>) -> String {
        let (min, max) = bounds(s);
        let long = s.type_name() == "long";
        let (tmin, tmax) = match s.type_name() {
            "long" => (i64::MIN, i64::MAX),
            "short" => (i16::MIN as i64, i16::MAX as i64),
            "byte" => (i8::MIN as i64, i8::MAX as i64),
            _ => (i32::MIN as i64, i32::MAX as i64),
        };
        let lo = min.unwrap_or(tmin).max(tmin);
        let hi = max.unwrap_or(tmax).min(tmax);
        if lo > hi {
            return lo.to_string();
        }
        let mut pool: Vec<i64> = vec![0, 1, -1, 7, 42, 1000, lo, hi, tmin, tmax];
        if long && self.dir == Dir::Output {
            pool.extend([9_007_199_254_740_993, 1 << 53, -(1 << 53) - 1]);
        }
        pool.push(self.rng.range(lo.max(-100_000), hi.min(100_000)));
        let pool: Vec<i64> = pool.into_iter().filter(|v| (lo..=hi).contains(v)).collect();
        if pool.is_empty() {
            return lo.to_string();
        }
        self.rng.pick(&pool).to_string()
    }

    fn float(&mut self, s: &Shape<'_>, ctx: Ctx) -> FloatLit {
        let (min, max) = float_bounds(s);
        let mut pool: Vec<(f64, &str)> = vec![
            (0.0, "0.0"),
            (1.5, "1.50"),
            (100.0, "1e2"),
            (-0.0, "-0.0"),
            (0.1, "0.1"),
            (1_234_567.0, "1234567.0"),
            (1e16, "1e16"),
            (1.797_693_134_862_315_7e308, "1.7976931348623157e308"),
            (5e-324, "5e-324"),
            (123_456_789.125, "123456789.125"),
            (-2.5, "-2.5E0"),
            (3.0, "3"),
            (0.000_123, "0.000123"),
            (12.0, "12.000"),
        ];
        let non_finite = self.dir == Dir::Output
            && !ctx.header
            && matches!(self.proto, Proto::Json | Proto::RestJson | Proto::Cbor);
        if non_finite {
            pool.extend([
                (f64::NAN, "NaN"),
                (f64::INFINITY, "Infinity"),
                (f64::NEG_INFINITY, "-Infinity"),
            ]);
        }
        let pool: Vec<(f64, &str)> = pool
            .into_iter()
            .filter(|(v, _)| {
                v.is_nan() || (min.is_none_or(|m| *v >= m) && max.is_none_or(|m| *v <= m))
            })
            .collect();
        if pool.is_empty() {
            let v = min.or(max).unwrap_or(0.0);
            return FloatLit {
                value: v,
                text: format!("{v:?}"),
            };
        }
        let (v, t) = *self.rng.pick(&pool);
        FloatLit {
            value: v,
            text: t.to_string(),
        }
    }

    pub fn timestamp(&mut self) -> Ts {
        let secs = if self.rng.chance(1, 2) {
            *self.rng.pick(DATE_SECS)
        } else {
            self.rng.range(0, 4_102_444_799)
        };
        let micros = match self.rng.below(4) {
            0 | 1 => 0,
            2 => self.rng.below(1000) as u32 * 1000,
            _ => self.rng.below(1_000_000) as u32,
        };
        Ts {
            secs,
            micros,
            form: self.rng.next_u64() as u32,
        }
    }

    fn doc(&mut self, depth: usize) -> Json {
        // A top-level null would read as an absent member (and an empty
        // union).
        let kind = match depth {
            0 => 1 + self.rng.below(6),
            1 => self.rng.below(7),
            _ => self.rng.below(5),
        };
        match kind {
            0 => Json::Null,
            1 => Json::Bool(self.rng.chance(1, 2)),
            2 => Json::from(self.rng.range(-1000, 1_000_000)),
            3 => serde_json::from_str(self.rng.pick(&["1.5", "0.25", "-3.0", "1e2"])).unwrap(),
            4 => Json::String(self.rng.pick(PLAIN_STRINGS).to_string()),
            5 => Json::Array(
                (0..self.rng.below(3))
                    .map(|_| self.doc(depth + 1))
                    .collect(),
            ),
            _ => {
                let mut m = JsonMap::new();
                for i in 0..self.rng.below(3) {
                    let k = format!("{}{i}", self.rng.pick(&["key", "名前", "Name"]));
                    m.insert(k, self.doc(depth + 1));
                }
                Json::Object(m)
            }
        }
    }
}

fn fit_len(candidate: String, min: usize, max: usize, g: &mut Generator<'_>) -> String {
    let n = candidate.chars().count();
    if n >= min && n <= max {
        return candidate;
    }
    let target = min.max(1).min(max).max(min);
    if n > max {
        return candidate.chars().take(max).collect();
    }
    let extra = target - n;
    let mut s = candidate;
    s.push_str(&g.alnum(extra));
    s
}

/// The shape type botocore sees. `models/` lacks the
/// `service-2.sdk-extras.json` merges, one of which changes a type.
pub fn type_of<'a>(s: &Shape<'a>) -> &'a str {
    if s.model.name == "s3" && s.name == "Expires" {
        return "timestamp";
    }
    s.type_name()
}

fn as_i64(v: &Node) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// The `min` / `max` traits of a shape (member reference first).
fn bounds(s: &Shape<'_>) -> (Option<i64>, Option<i64>) {
    (s.get("min").and_then(as_i64), s.get("max").and_then(as_i64))
}

fn float_bounds(s: &Shape<'_>) -> (Option<f64>, Option<f64>) {
    (
        s.get("min").and_then(Node::as_f64),
        s.get("max").and_then(Node::as_f64),
    )
}

impl G {
    pub fn get(&self, key: &str) -> Option<&G> {
        match self {
            G::Struct(m) | G::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut G> {
        match self {
            G::Struct(m) | G::Map(m) => m.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<G> {
        match self {
            G::Struct(m) | G::Map(m) => {
                let i = m.iter().position(|(k, _)| k == key)?;
                Some(m.remove(i).1)
            }
            _ => None,
        }
    }

    pub fn set(&mut self, key: &str, v: G) {
        if let G::Struct(m) | G::Map(m) = self {
            if let Some(slot) = m.iter_mut().find(|(k, _)| k == key) {
                slot.1 = v;
            } else {
                m.push((key.to_string(), v));
            }
        }
    }
}
