//! `awscli/redaction.py`: PEM private keys and base64 encoded PEM private
//! keys in history payloads.

use std::sync::OnceLock;

use base64::Engine;
use regex::bytes::{Captures, Regex};

use crate::value::Value;

const REDACTED_PRIVATE_KEY: &[u8] = b"<redacted private key>";

/// `_PRIVATE_KEY_PATTERN`.
fn private_key() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s-u)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
        )
        .expect("regex")
    })
}

/// The base64 pattern without its lookbehind `(?<![A-Za-z0-9+/])`, which
/// [`redact_base64`] checks.
fn base64_pem() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"LS0tLS1CRUdJTi[A-Za-z0-9+/]*={0,2}").expect("regex"))
}

/// `base64.b64decode` without `validate`: trailing bits and padding are
/// not checked.
fn lenient() -> &'static base64::engine::GeneralPurpose {
    static ENGINE: OnceLock<base64::engine::GeneralPurpose> = OnceLock::new();
    ENGINE.get_or_init(|| {
        base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new()
                .with_decode_allow_trailing_bits(true)
                .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
        )
    })
}

/// `redact(value)`: strings and bytes, also inside lists and mappings.
pub(crate) fn redact(value: Value) -> Value {
    match value {
        Value::Str(s) => match redact_bytes(s.as_bytes()) {
            Some(r) => Value::Str(String::from_utf8(r).unwrap_or(s)),
            None => Value::Str(s),
        },
        Value::Bytes(b) => match redact_bytes(&b) {
            Some(r) => Value::Bytes(r),
            None => Value::Bytes(b),
        },
        Value::List(l) => Value::List(l.into_iter().map(redact).collect()),
        Value::Map(m) => Value::Map(m.into_iter().map(|(k, v)| (k, redact(v))).collect()),
        v => v,
    }
}

/// `_redact_text`, or `None` when nothing matched.
fn redact_bytes(text: &[u8]) -> Option<Vec<u8>> {
    let first = private_key().is_match(text);
    let text = if first {
        private_key()
            .replace_all(text, REDACTED_PRIVATE_KEY)
            .into_owned()
    } else {
        text.to_vec()
    };
    match redact_base64(&text) {
        Some(r) => Some(r),
        None if first => Some(text),
        None => None,
    }
}

/// The base64 pattern with `_redact_base64_private_key`.
fn redact_base64(text: &[u8]) -> Option<Vec<u8>> {
    if !base64_pem().is_match(text) {
        return None;
    }
    let out = base64_pem().replace_all(text, |c: &Captures<'_>| {
        let m = c.get(0).expect("match");
        let preceded = m.start() > 0
            && matches!(text[m.start() - 1], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/');
        let encoded = m.as_bytes();
        if preceded {
            return encoded.to_vec();
        }
        let usable = &encoded[..encoded.len() / 4 * 4];
        match lenient().decode(usable) {
            Ok(decoded) if private_key().is_match(&decoded) => REDACTED_PRIVATE_KEY.to_vec(),
            _ => encoded.to_vec(),
        }
    });
    Some(out.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_private_keys() {
        let kind = "RSA PRIVATE KEY";
        let pem = format!("a -----BEGIN {kind}-----\nMIIE\n-----END {kind}----- b");
        assert_eq!(
            redact(Value::Str(pem)),
            Value::Str("a <redacted private key> b".to_string())
        );
        let kind = "PRIVATE KEY";
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("-----BEGIN {kind}-----\nx\n-----END {kind}-----\n"));
        assert_eq!(
            redact(Value::Str(format!("k={encoded}"))),
            Value::Str("k=<redacted private key>".to_string())
        );
        let cert = base64::engine::general_purpose::STANDARD.encode("-----BEGIN CERTIFICATE-----");
        assert_eq!(redact(Value::Str(cert.clone())), Value::Str(cert));
    }
}
