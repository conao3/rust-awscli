//! Differential tests against goldens produced by the reference botocore
//! (`generate.py`).

use super::common::testutil::dump;
use super::{HttpResponse, ParseOptions, TimestampFormat};
use crate::model::ServiceModel;
use crate::value::Value;

mod goldens;

pub struct SerCase {
    pub service: &'static str,
    pub operation: &'static str,
    pub params: &'static str,
    pub method: &'static str,
    pub url_path: &'static str,
    pub query: &'static [(&'static str, &'static str)],
    pub headers: &'static [(&'static str, &'static str)],
    pub body: &'static [u8],
    pub host_prefix: Option<&'static str>,
}

pub struct ParseCase {
    pub service: &'static str,
    pub operation: &'static str,
    pub status: u16,
    pub headers: &'static [(&'static str, &'static str)],
    pub body: &'static [u8],
    pub wire: bool,
    pub expected: &'static str,
}

fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
    p.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn serialize_matches_botocore() {
    let mut failures = Vec::new();
    for c in goldens::SER_CASES {
        let model = ServiceModel::load(c.service).unwrap();
        let op = model.operation(c.operation).unwrap();
        let params = Value::parse_json(c.params).unwrap();
        let label = format!("{} {}", c.service, c.operation);
        let req = match super::serialize(&model, &op, &params) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{label}: {e:#}"));
                continue;
            }
        };
        let checks = [
            ("method", req.method == c.method),
            ("url_path", req.url_path == c.url_path),
            ("query", req.query == pairs(c.query)),
            ("headers", req.headers == pairs(c.headers)),
            ("body", req.body == c.body),
            ("host_prefix", req.host_prefix.as_deref() == c.host_prefix),
        ];
        for (field, ok) in checks {
            if !ok {
                failures.push(format!(
                    "{label}: {field} differs\n  got:  {:?} {:?} {:?} {:?} {:?}\n  body: {}\n  want: {:?} {:?} {:?} {:?} {:?}\n  body: {}",
                    req.method,
                    req.url_path,
                    req.query,
                    req.headers,
                    req.host_prefix,
                    String::from_utf8_lossy(&req.body),
                    c.method,
                    c.url_path,
                    c.query,
                    c.headers,
                    c.host_prefix,
                    String::from_utf8_lossy(c.body),
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn parse_matches_botocore() {
    let mut failures = Vec::new();
    for c in goldens::PARSE_CASES {
        let model = ServiceModel::load(c.service).unwrap();
        let op = model.operation(c.operation).unwrap();
        let response = HttpResponse {
            status: c.status,
            headers: pairs(c.headers),
            body: c.body.to_vec(),
        };
        let options = ParseOptions {
            timestamp_format: if c.wire {
                TimestampFormat::Wire
            } else {
                TimestampFormat::Iso8601
            },
        };
        let label = format!("{} {} {}", c.service, c.operation, c.status);
        match super::parse(&model, &op, &response, &options) {
            Ok(v) => {
                let got = dump(&v);
                if got != c.expected {
                    failures.push(format!("{label}\n  got:  {got}\n  want: {}", c.expected));
                }
            }
            Err(e) => failures.push(format!("{label}: {e:#}")),
        }
        if c.status >= 300 {
            let err = super::parse_error(&model, &op, &response, &options).unwrap();
            assert_eq!(dump(&err.parsed), c.expected, "{label}");
            let code = err.parsed.get("Error").and_then(|e| e.get("Code"));
            if let Some(Value::Str(s)) = code {
                assert_eq!(&err.code, s, "{label}");
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
