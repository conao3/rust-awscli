//! Rendering errors for stderr (`awscli/errorhandler.py`,
//! `awscli/errorformat.py`, `botocore/exceptions.py`).

use crate::pyfmt::{self, py_str};
use crate::value::{Map, Value};

use super::{Color, table, text, yaml};

/// A service error to report (`botocore.exceptions.ClientError`).
#[derive(Debug, Clone)]
pub struct ClientErrorInfo {
    pub operation_name: String,
    /// `Error.Code` of the parsed response (`Unknown` when absent).
    pub code: String,
    /// `Error.Message` of the parsed response (`Unknown` when absent).
    pub message: String,
    /// `(reached max retries: N)` is appended when the request was retried
    /// until the attempt limit; this is `N`.
    pub max_retries_reached: Option<u32>,
    /// The full parsed error response (for the `json` / `yaml` / `text` /
    /// `table` / `enhanced` error formats).
    pub parsed: Value,
    /// `ClientError.modeled_fields`: `Code`, `Message`, and the members of
    /// the error shape (botocore `client.py` `_make_api_call`); `None` when
    /// unknown (only `Code` and `Message` are shown then).
    pub modeled_fields: Option<Vec<String>>,
}

impl ClientErrorInfo {
    /// `str(ClientError)` (`ClientError.MSG_TEMPLATE`).
    pub fn exception_message(&self) -> String {
        let retry_info = match self.max_retries_reached {
            Some(n) => format!(" (reached max retries: {n})"),
            None => String::new(),
        };
        format!(
            "An error occurred ({}) when calling the {} operation{}: {}",
            self.code, self.operation_name, retry_info, self.message
        )
    }
}

/// How stdout is attached, which the structured error formats consult
/// (`--color`, and the `table` format's terminal width and tty check read
/// stdout, not stderr).
#[derive(Debug, Clone, Copy)]
pub struct ErrorOutputOptions {
    pub color: Color,
    pub stdout_is_tty: bool,
    pub terminal_width: Option<usize>,
}

impl Default for ErrorOutputOptions {
    fn default() -> Self {
        ErrorOutputOptions {
            color: Color::Auto,
            stdout_is_tty: false,
            terminal_width: None,
        }
    }
}

/// Render a client error in `error_format` (`legacy`, `json`, `yaml`,
/// `text`, `table`, `enhanced`), exactly as the AWS CLI writes it to
/// stderr.
pub fn render_client_error(info: &ClientErrorInfo, error_format: &str) -> String {
    render_client_error_with(info, error_format, &ErrorOutputOptions::default())
}

/// [`render_client_error`] with the stdout state the `table` format
/// depends on (`ClientErrorHandler._do_handle_exception`).
pub fn render_client_error_with(
    info: &ClientErrorInfo,
    error_format: &str,
    options: &ErrorOutputOptions,
) -> String {
    let message = info.exception_message();
    if let Some(error_info) = extract_error_info(info)
        && !error_info.is_empty()
        && let Some(out) = display_structured_error(&error_info, &message, error_format, options)
    {
        return out;
    }
    render_general_error(&message)
}

/// `ClientErrorHandler._extract_error_response` / `_extract_error_info`.
fn extract_error_info(info: &ClientErrorInfo) -> Option<Map> {
    let response = info.parsed.as_map()?;
    let error = response.get("Error")?;
    let mut error_dict = match error {
        Value::Map(m) => m.clone(),
        _ => return None,
    };
    let modeled_lower: Vec<String> = match &info.modeled_fields {
        Some(fields) => fields.iter().map(|f| f.to_lowercase()).collect(),
        None => vec!["code".to_string(), "message".to_string()],
    };
    let is_modeled = |k: &str| modeled_lower.iter().any(|m| *m == k.to_lowercase());
    for (key, value) in response {
        if matches!(
            key.as_str(),
            "Error" | "ResponseMetadata" | "Code" | "Message"
        ) {
            continue;
        }
        if !error_dict.contains_key(key) && is_modeled(key) {
            error_dict.insert(key.clone(), value.clone());
        }
    }
    error_dict.retain(|k, _| is_modeled(k));
    Some(error_dict)
}

/// `FilteredExceptionHandler._display_structured_error`; `None` means
/// the legacy path.
fn display_structured_error(
    error_info: &Map,
    formatted_message: &str,
    error_format: &str,
    options: &ErrorOutputOptions,
) -> Option<String> {
    let mut error_format = error_format.to_lowercase();
    if error_format == "legacy" {
        return None;
    }
    if !matches!(
        error_format.as_str(),
        "json" | "yaml" | "text" | "table" | "enhanced"
    ) {
        error_format = "enhanced".to_string();
    }
    if error_format == "enhanced" {
        let mut out = render_general_error(formatted_message);
        format_enhanced(error_info, &mut out);
        return Some(out);
    }
    let data = Value::Map(error_info.clone());
    let mut out = String::new();
    match error_format.as_str() {
        "json" => {
            out.push_str(&pyfmt::json_dumps_pretty(&data));
            out.push('\n');
        }
        "yaml" => yaml::dump(&data, &mut out),
        "text" => {
            if text::format_text(&data, &mut out).is_err() {
                // The formatter has written part of its output before the
                // handler falls back to the legacy message.
                out.push_str(&render_general_error(formatted_message));
            }
        }
        "table" => {
            let colorized = match options.color {
                Color::Auto => options.stdout_is_tty,
                Color::On => true,
                Color::Off => false,
            };
            let styler = if colorized {
                table::Styler::Colorized
            } else {
                table::Styler::Plain
            };
            // The handler's stderr is not the colorama wrapper, so writes
            // are not followed by resets.
            super::render_table(
                "error",
                &data,
                styler,
                false,
                options.terminal_width.unwrap_or(80),
                &mut out,
            )
            .ok()?;
        }
        _ => return None,
    }
    Some(out)
}

const MAX_INLINE_ITEMS: usize = 5;

fn is_simple_value(v: &Value) -> bool {
    matches!(
        v,
        Value::Str(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null
    )
}

/// `EnhancedErrorFormatter.format_error`.
fn format_enhanced(error_info: &Map, out: &mut String) {
    let additional: Vec<(&String, &Value)> = error_info
        .iter()
        .filter(|(k, _)| !matches!(k.to_lowercase().as_str(), "code" | "message"))
        .collect();
    if additional.is_empty() {
        return;
    }
    out.push_str("\nAdditional error details:\n");
    let mut has_complex_value = false;
    for (key, value) in additional {
        if is_simple_value(value) {
            out.push_str(&format!("{key}: {}\n", py_str(value)));
        } else if is_small_collection(value) {
            out.push_str(&format!("{key}: {}\n", format_inline(value)));
        } else {
            out.push_str(&format!("{key}: <complex value>\n"));
            has_complex_value = true;
        }
    }
    if has_complex_value {
        out.push_str(
            "Use \"--cli-error-format json\" or another error format to see the full details.\n",
        );
    }
}

fn is_small_collection(v: &Value) -> bool {
    match v {
        Value::List(l) => l.len() < MAX_INLINE_ITEMS && l.iter().all(is_simple_value),
        Value::Map(m) => m.len() < MAX_INLINE_ITEMS && m.values().all(is_simple_value),
        _ => false,
    }
}

fn format_inline(v: &Value) -> String {
    match v {
        Value::List(l) => format!("[{}]", l.iter().map(py_str).collect::<Vec<_>>().join(", ")),
        Value::Map(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{k}: {}", py_str(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => py_str(other),
    }
}

/// `errorformat.write_error`: `"\naws: [ERROR]: <message>\n"` (with the
/// exact surrounding newlines of the AWS CLI).
pub fn render_general_error(message: &str) -> String {
    format!("\n{}\n", format_error_message(message))
}

/// `errorformat.format_error_message` (`str.strip()` of the message).
pub fn format_error_message(message: &str) -> String {
    if message.is_empty() {
        return "aws: [ERROR]:".to_string();
    }
    format!("aws: [ERROR]: {}", py_strip(message))
}

/// Python `str.strip()` with no arguments (strips Unicode whitespace).
fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_whitespace)
}

/// Python `str.isspace()` for one character.
fn is_py_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{b}'
            | '\u{c}'
            | '\r'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{1f}'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// `FilteredExceptionHandler._do_handle_exception` for errors whose
/// handler extracts `{'Code': code, 'Message': message}` (no credentials,
/// no region, configuration, pager errors).
pub fn render_structured_error(
    code: &str,
    message: &str,
    error_format: &str,
    options: &ErrorOutputOptions,
) -> String {
    let mut error_info = Map::new();
    error_info.insert("Code".to_string(), Value::str(code));
    error_info.insert("Message".to_string(), Value::str(message));
    let formatted = format!("An error occurred ({code}): {message}");
    display_structured_error(&error_info, &formatted, error_format, options)
        .unwrap_or_else(|| render_general_error(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::goldens;
    use crate::output::test_support::decode;

    #[test]
    fn client_error_goldens() {
        let mut failures = Vec::new();
        for &(op, code, message, retries, parsed, modeled, fmt, color, tty, width, expected) in
            goldens::CLIENT_ERROR
        {
            let info = ClientErrorInfo {
                operation_name: op.to_string(),
                code: code.to_string(),
                message: message.to_string(),
                max_retries_reached: retries,
                parsed: decode(parsed),
                modeled_fields: modeled.map(|m| {
                    decode(m)
                        .as_list()
                        .expect("modeled fields")
                        .iter()
                        .map(|v| v.as_str().expect("field").to_string())
                        .collect()
                }),
            };
            let options = ErrorOutputOptions {
                color: match color {
                    "on" => Color::On,
                    "off" => Color::Off,
                    _ => Color::Auto,
                },
                stdout_is_tty: tty,
                terminal_width: Some(width),
            };
            let actual = render_client_error_with(&info, fmt, &options);
            if actual != expected {
                failures.push(format!(
                    "{op} {fmt} {color} tty={tty} w={width}\n--- expected\n{expected}\n--- actual\n{actual}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn general_error_goldens() {
        for &(message, expected) in goldens::GENERAL_ERROR {
            assert_eq!(render_general_error(message), expected, "{message:?}");
        }
    }
}
