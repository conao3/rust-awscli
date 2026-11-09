//! `awscli/customizations/history/show.py` `DetailedFormatter` and
//! `SectionValuePrettyFormatter`.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

use crate::value::{Map, Value};

/// A row of `records` with its payload loaded.
pub(crate) struct Record {
    pub request_id: Option<String>,
    pub event_type: String,
    pub timestamp: i64,
    pub payload: Value,
}

/// How a value of a section is shown.
#[derive(Clone, Copy)]
enum Format {
    Plain,
    Dictionary,
    HttpBody,
}

/// One value of a section: its description, its payload key, its format,
/// and whether the signature filter applies.
type ValueDef = (&'static str, Option<&'static str>, Format, bool);

/// `DetailedFormatter._SECTIONS`.
fn section(event_type: &str) -> Option<(Option<&'static str>, &'static [ValueDef])> {
    Some(match event_type {
        "CLI_VERSION" => (
            Some("AWS CLI command entered"),
            &[("with AWS CLI version", None, Format::Plain, false)],
        ),
        "CLI_ARGUMENTS" => (None, &[("with arguments", None, Format::Plain, false)]),
        "API_CALL" => (
            Some("API call made"),
            &[
                ("to service", Some("service"), Format::Plain, false),
                ("using operation", Some("operation"), Format::Plain, false),
                ("with parameters", Some("params"), Format::Dictionary, false),
            ],
        ),
        "HTTP_REQUEST" => (
            Some("HTTP request sent"),
            &[
                ("to URL", Some("url"), Format::Plain, false),
                ("with method", Some("method"), Format::Plain, false),
                ("with headers", Some("headers"), Format::Dictionary, true),
                ("with body", Some("body"), Format::HttpBody, false),
            ],
        ),
        "HTTP_RESPONSE" => (
            Some("HTTP response received"),
            &[
                (
                    "with status code",
                    Some("status_code"),
                    Format::Plain,
                    false,
                ),
                ("with headers", Some("headers"), Format::Dictionary, false),
                ("with body", Some("body"), Format::HttpBody, false),
            ],
        ),
        "PARSED_RESPONSE" => (
            Some("HTTP response parsed"),
            &[("parsed to", None, Format::Dictionary, false)],
        ),
        "CLI_RC" => (
            Some("AWS CLI command exited"),
            &[("with return code", None, Format::Plain, false)],
        ),
        _ => return None,
    })
}

/// `_SIG_FILTER`.
fn signature_filter() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("Signature=([a-z0-9]{4})[a-z0-9]{60}").expect("regex"))
}

const BRIGHT: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";
const RESET_ALL: &str = "\x1b[0m";

/// `DetailedFormatter`.
pub(crate) struct DetailedFormatter {
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    colorize: bool,
    request_id_to_api_num: HashMap<String, usize>,
    num_api_calls: usize,
}

impl DetailedFormatter {
    pub(crate) fn new(
        include: Option<Vec<String>>,
        exclude: Option<Vec<String>>,
        colorize: bool,
    ) -> DetailedFormatter {
        DetailedFormatter {
            include,
            exclude,
            colorize,
            request_id_to_api_num: HashMap::new(),
            num_api_calls: 0,
        }
    }

    /// `Formatter.display`: `Err` is the `ExpatError` the pretty printing
    /// of an XML body raises, after the values before it are written.
    pub(crate) fn display(&mut self, record: &Record, out: &mut String) -> Result<(), String> {
        let shown = match (&self.include, &self.exclude) {
            (Some(i), _) if !i.is_empty() => i.contains(&record.event_type),
            (_, Some(e)) if !e.is_empty() => !e.contains(&record.event_type),
            _ => true,
        };
        if !shown {
            return Ok(());
        }
        let Some((title, values)) = section(&record.event_type) else {
            return Ok(());
        };
        if let Some(title) = title {
            self.title(title, record, out);
        }
        for (description, key, format, filtered) in values {
            let value = match key {
                Some(k) => record.payload.get(k).cloned().unwrap_or_default(),
                None => record.payload.clone(),
            };
            let mut text = self.description(description);
            text.push_str(&format_value(&value, record, *format)?);
            if *filtered {
                text = signature_filter()
                    .replace_all(&text, "Signature=${1}...")
                    .into_owned();
            }
            out.push_str(&text);
        }
        Ok(())
    }

    fn title(&mut self, title: &str, record: &Record, out: &mut String) {
        let mut formatted = title.to_string();
        if let Some(num) = self.api_num(record) {
            formatted = format!("[{num}] {formatted}");
        }
        formatted = self.color(&formatted, BRIGHT);
        out.push('\n');
        out.push_str(&formatted);
        out.push('\n');
        out.push_str(&self.description("at time"));
        out.push_str(&timestamp(record.timestamp));
        out.push('\n');
    }

    fn api_num(&mut self, record: &Record) -> Option<usize> {
        let id = record.request_id.as_ref().filter(|r| !r.is_empty())?;
        if !self.request_id_to_api_num.contains_key(id) {
            self.request_id_to_api_num
                .insert(id.clone(), self.num_api_calls);
            self.num_api_calls += 1;
        }
        self.request_id_to_api_num.get(id).copied()
    }

    fn description(&self, description: &str) -> String {
        self.color(&format!("{description}: "), CYAN)
    }

    fn color(&self, text: &str, code: &str) -> String {
        if self.colorize {
            format!("{code}{text}{RESET_ALL}")
        } else {
            text.to_string()
        }
    }
}

/// `_format_value`.
fn format_value(value: &Value, record: &Record, format: Format) -> Result<String, String> {
    let mut out = match format {
        Format::Plain => crate::pyfmt::py_str(value),
        Format::Dictionary => dictionary(value),
        Format::HttpBody => http_body(value, record)?,
    };
    out.push('\n');
    Ok(out)
}

/// `_pformat_timestamp`: local time with milliseconds.
fn timestamp(ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        None => ms.to_string(),
    }
}

/// `_pformat_dictionary`: `json.dumps(obj, sort_keys=True, indent=4)`.
pub(crate) fn dictionary(value: &Value) -> String {
    crate::pyfmt::json_dumps_indent_ascii(&sorted(value))
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Map(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), sorted(&m[k]));
            }
            Value::Map(out)
        }
        Value::List(l) => Value::List(l.iter().map(sorted).collect()),
        v => v.clone(),
    }
}

/// `_pformat_http_body`.
fn http_body(body: &Value, record: &Record) -> Result<String, String> {
    if !body.is_truthy() {
        return Ok("There is no associated body".to_string());
    }
    if record
        .payload
        .get("streaming")
        .is_some_and(Value::is_truthy)
    {
        return Ok("The body is a stream and will not be displayed".to_string());
    }
    let text = crate::pyfmt::py_str(body);
    if let Some(pretty) = super::minidom::pretty_xml(&text) {
        return pretty;
    }
    if text.starts_with('{')
        && let Ok(obj) = crate::cli::pyjson::loads(&text)
    {
        return Ok(dictionary(&obj));
    }
    Ok(text)
}
