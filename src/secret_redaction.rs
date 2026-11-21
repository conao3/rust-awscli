//! The credential redaction the agent-registry customization applies to
//! the `--debug` log handlers and the history recorder
//! (`awscli/customizations/agentregistry/utils.py` `redact_secrets`,
//! `_RedactSecrets`, `_redacted`, `_redact_history`,
//! `redact_logs_and_history`). Nothing is redacted until a command
//! activates it.

use std::collections::BTreeSet;
use std::sync::{LazyLock, Mutex};

use regex::Regex;

use crate::value::Value;

/// `_REDACTED`.
pub(crate) const REDACTED: &str = "<redacted>";

/// `_MIN_SECRET_LENGTH`.
const MIN_SECRET_LENGTH: usize = 8;

/// `_TOKEN_FIELDS`.
const TOKEN_FIELDS: [&str; 6] = [
    "RefreshToken",
    "AccessToken",
    "IdToken",
    "refresh_token",
    "access_token",
    "id_token",
];

struct State {
    active: bool,
    secrets: BTreeSet<String>,
}

static STATE: Mutex<State> = Mutex::new(State {
    active: false,
    secrets: BTreeSet::new(),
});

fn names() -> String {
    TOKEN_FIELDS.join("|")
}

static BEARER_CREDENTIAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(Bearer\s+)[A-Za-z0-9\-._~+/]+=*").expect("regex"));
static COMPACT_JWT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"eyJ[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]*){2,4}").expect("regex"));
static PRESIGNED_URL_CREDENTIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)((?:X-Amz-Signature|X-Amz-Credential|X-Amz-Security-Token)=)[^&\s'"\\]+"#)
        .expect("regex")
});
static JSON_TOKEN_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r#"("(?:{})"\s*:\s*")(?:[^"\\]|\\.)*(")"#, names())).expect("regex")
});
static REPR_TOKEN_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"('(?:{})'\s*:\s*')(?:[^'\\]|\\.)*(')", names())).expect("regex")
});
static FORM_TOKEN_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r#"(\b(?:{})=)[^&\s'"\\]*"#, names())).expect("regex"));

/// `redact_logs_and_history(*secrets)`: from now on, redact the log
/// records and history payloads, removing `secrets` as well.
pub(crate) fn activate(secrets: &[&str]) {
    let mut state = STATE.lock().unwrap();
    state.active = true;
    for s in secrets {
        if s.chars().count() >= MIN_SECRET_LENGTH {
            state.secrets.insert(s.to_string());
        }
    }
}

/// `redact_secrets(text, secrets)`.
pub(crate) fn redact_secrets(text: &str, secrets: &BTreeSet<String>) -> String {
    let mut ordered: Vec<&String> = secrets.iter().collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
    let mut text = text.to_string();
    for secret in ordered {
        text = text.replace(secret.as_str(), REDACTED);
    }
    let text = JSON_TOKEN_FIELD.replace_all(&text, format!("${{1}}{REDACTED}${{2}}"));
    let text = REPR_TOKEN_FIELD.replace_all(&text, format!("${{1}}{REDACTED}${{2}}"));
    let text = FORM_TOKEN_FIELD.replace_all(&text, format!("${{1}}{REDACTED}"));
    let text = BEARER_CREDENTIAL.replace_all(&text, format!("${{1}}{REDACTED}"));
    let text = COMPACT_JWT.replace_all(&text, REDACTED);
    PRESIGNED_URL_CREDENTIAL
        .replace_all(&text, format!("${{1}}{REDACTED}"))
        .into_owned()
}

/// `_RedactSecrets.filter`: the message of a log record, once active.
pub(crate) fn log_message(message: &str) -> std::borrow::Cow<'_, str> {
    let state = STATE.lock().unwrap();
    if !state.active {
        return std::borrow::Cow::Borrowed(message);
    }
    std::borrow::Cow::Owned(redact_secrets(message, &state.secrets))
}

fn redacted(value: Value, secrets: &BTreeSet<String>) -> Value {
    match value {
        Value::Str(s) => Value::Str(redact_secrets(&s, secrets)),
        Value::Bytes(b) => match String::from_utf8(b) {
            Ok(text) => {
                let r = redact_secrets(&text, secrets);
                Value::Bytes(r.into_bytes())
            }
            Err(e) => Value::Bytes(e.into_bytes()),
        },
        Value::Map(m) => Value::Map(
            m.into_iter()
                .map(|(k, v)| {
                    let v = if TOKEN_FIELDS.contains(&k.as_str())
                        && matches!(v, Value::Str(_) | Value::Bytes(_))
                    {
                        Value::str(REDACTED)
                    } else {
                        redacted(v, secrets)
                    };
                    (k, v)
                })
                .collect(),
        ),
        Value::List(l) => Value::List(l.into_iter().map(|v| redacted(v, secrets)).collect()),
        v => v,
    }
}

/// `_redact_history`'s `redacting_record`: the payload of a history
/// record, once active.
pub(crate) fn history_payload(payload: Value) -> Value {
    let state = STATE.lock().unwrap();
    if !state.active {
        return payload;
    }
    redacted(payload, &state.secrets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts() {
        let secrets: BTreeSet<String> = ["opaque-secret-token".to_string()].into();
        let cases = [
            (
                "Authorization: Bearer abc.def-ghi",
                "Authorization: Bearer <redacted>",
            ),
            (
                r#"{"refresh_token": "x\"y", "a": "eyJhbGciOi.eyJzdWIi.c2ln"}"#,
                r#"{"refresh_token": "<redacted>", "a": "<redacted>"}"#,
            ),
            ("{'AccessToken': 'abc'}", "{'AccessToken': '<redacted>'}"),
            (
                "grant_type=refresh_token&refresh_token=abc&client_id=x",
                "grant_type=refresh_token&refresh_token=<redacted>&client_id=x",
            ),
            (
                "https://b.s3.amazonaws.com/k?X-Amz-Credential=AK%2F1&x-amz-signature=ff&a=1",
                "https://b.s3.amazonaws.com/k?X-Amz-Credential=<redacted>&x-amz-signature=<redacted>&a=1",
            ),
            ("token opaque-secret-token here", "token <redacted> here"),
        ];
        for (text, expected) in cases {
            assert_eq!(redact_secrets(text, &secrets), expected, "{text}");
        }
    }
}
