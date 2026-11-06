//! Normalizing and comparing two runs: stdout, stderr, exit code, the
//! streaming output file, and the recorded requests with volatile values
//! masked.

use std::fmt::Write as _;
use std::sync::OnceLock;

use regex::Regex;
use similar::TextDiff;

use crate::run::RunOut;
use crate::server::Recorded;

/// Headers whose values depend on time, credentials, or the client
/// implementation.
const MASKED_HEADERS: &[&str] = &[
    "authorization",
    "x-amz-date",
    "x-amz-content-sha256",
    "x-amz-security-token",
    "user-agent",
    "x-amz-user-agent",
    "amz-sdk-invocation-id",
];

fn uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new("[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}").unwrap()
    })
}

/// The time-dependent parts of presigned URLs in request bodies (the
/// cross-region `PresignedUrl` / `PreSignedUrl` of ec2 and rds), plain or
/// percent-encoded, and of SigV2-signed query bodies (sdb).
fn presigned_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new("(X-Amz-Date(?:=|%3D))[0-9]{8}T[0-9]{6}Z|(X-Amz-Signature(?:=|%3D))[0-9a-f]{64}|(&Timestamp=)[^&\\s]*|(&Signature=)[^&\\s]*|(X-Amz-Credential(?:=|%3D)[^&/%]*(?:/|%2F|%252F))[0-9]{8}")
            .unwrap()
    })
}

/// Make control characters visible so diffs of terminal output and
/// binary data stay readable.
pub fn visible(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        out.push_str("\n\\ no newline at end\n");
    }
    out
}

fn body_text(content_type: &str, body: &[u8]) -> String {
    if body.is_empty() {
        return String::new();
    }
    if content_type.contains("cbor")
        && let Ok(v) = ciborium::de::from_reader::<ciborium::Value, _>(body)
    {
        return format!("{v:#?}\n");
    }
    match std::str::from_utf8(body) {
        Ok(s) => visible(s.as_bytes()),
        Err(_) => {
            let mut out = String::from("<binary>\n");
            for chunk in body.chunks(32) {
                for b in chunk {
                    let _ = write!(out, "{b:02x}");
                }
                out.push('\n');
            }
            out
        }
    }
}

/// The signing region and service of a SigV4 `Authorization` header (the
/// rest depends on the time and the signer).
fn masked_authorization(value: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"Credential=[^/]*/[0-9]{8}/([^/]+)/([^/]+)/aws4_request").unwrap()
    });
    match re.captures(value) {
        Some(c) => format!("<masked scope={}/{}>", &c[1], &c[2]),
        None => "<masked>".to_string(),
    }
}

pub fn render_request(r: &Recorded) -> String {
    let mut out = format!("{} {}\n", r.method, r.target);
    // A generated idempotency token in the body makes its checksums
    // differ between runs.
    let body_text_lossy = String::from_utf8_lossy(&r.body);
    let body_has_uuid = uuid_re().is_match(&body_text_lossy);
    let sigv2 = body_text_lossy.contains("SignatureVersion=2");
    let mut headers: Vec<(String, String)> = r
        .headers
        .iter()
        .map(|(k, v)| {
            let k = k.to_ascii_lowercase();
            let body_digest = k.starts_with("x-amz-checksum-") || k == "content-md5";
            let v = if k == "authorization" {
                masked_authorization(&String::from_utf8_lossy(v))
            } else if MASKED_HEADERS.contains(&k.as_str())
                || (body_has_uuid && body_digest)
                || (sigv2 && k == "content-length")
            {
                "<masked>".to_string()
            } else {
                String::from_utf8_lossy(v).into_owned()
            };
            (k, v)
        })
        .collect();
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let content_type = headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    for (k, v) in &headers {
        let _ = writeln!(out, "{k}: {v}");
    }
    out.push('\n');
    out.push_str(&body_text(&content_type, &r.body));
    let out = uuid_re().replace_all(&out, "<uuid>");
    presigned_re()
        .replace_all(&out, "${1}${2}${3}${4}${5}<masked>")
        .into_owned()
}

pub fn render_requests(reqs: &[Recorded]) -> String {
    let mut out = String::new();
    for (i, r) in reqs.iter().enumerate() {
        let _ = writeln!(out, "--- request {}", i + 1);
        out.push_str(&render_request(r));
    }
    out
}

/// One side of a comparison.
pub struct Side<'a> {
    pub run: &'a RunOut,
    pub requests: &'a [Recorded],
}

fn exit_text(r: &RunOut) -> String {
    if r.timed_out {
        "timed out\n".to_string()
    } else {
        format!("{:?}\n", r.code)
    }
}

/// Unified diffs of every part that differs; empty when the runs match.
pub fn compare(reference: &Side<'_>, candidate: &Side<'_>) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut part = |name: &str, a: String, b: String, same: bool| {
        if !same {
            let diff = TextDiff::from_lines(&a, &b)
                .unified_diff()
                .context_radius(3)
                .header("reference", "candidate")
                .to_string();
            out.push((name.to_string(), diff));
        }
    };
    let (a, b) = (reference.run, candidate.run);
    let a_stderr = match std::str::from_utf8(&a.stderr) {
        Ok(s) => awsr::progname::messages(s).into_bytes(),
        Err(_) => a.stderr.clone(),
    };
    part(
        "exit",
        exit_text(a),
        exit_text(b),
        a.code == b.code && a.timed_out == b.timed_out,
    );
    part(
        "stdout",
        visible(&a.stdout),
        visible(&b.stdout),
        a.stdout == b.stdout,
    );
    part(
        "stderr",
        visible(&a_stderr),
        visible(&b.stderr),
        a_stderr == b.stderr,
    );
    if a.outfile.is_some() || b.outfile.is_some() {
        let fa = a.outfile.clone().unwrap_or_default();
        let fb = b.outfile.clone().unwrap_or_default();
        part(
            "outfile",
            visible(&fa),
            visible(&fb),
            a.outfile == b.outfile,
        );
    }
    let ra = render_requests(reference.requests);
    let rb = render_requests(candidate.requests);
    let same = ra == rb;
    part("requests", ra, rb, same);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_volatile_headers_and_tokens() {
        let r = Recorded {
            method: "POST".into(),
            target: "/".into(),
            headers: vec![
                ("X-Amz-Date".into(), b"20260101T000000Z".to_vec()),
                (
                    "content-type".into(),
                    b"application/x-amz-json-1.1".to_vec(),
                ),
            ],
            body: br#"{"Request": "3f2c8e1a-1b2c-4d5e-8f90-0123456789ab"}"#.to_vec(),
        };
        let text = render_request(&r);
        assert!(text.contains("x-amz-date: <masked>"));
        assert!(text.contains(r#""Request": "<uuid>""#));

        let r = Recorded {
            method: "POST".into(),
            target: "/".into(),
            headers: vec![],
            body: format!(
                "PresignedUrl=http%3A%2F%2Fh%2F%3FX-Amz-Date%3D20261103T172307Z%26X-Amz-Signature%3D{}",
                "a".repeat(64)
            )
            .into_bytes(),
        };
        let text = render_request(&r);
        assert!(text.contains("X-Amz-Date%3D<masked>%26X-Amz-Signature%3D<masked>"));
    }
}
