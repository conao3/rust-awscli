//! Output formatting (`awscli/formatter.py`, `awscli/table.py`,
//! `awscli/text.py`).

use std::io::Write;

use anyhow::{Result, anyhow};

use crate::jmes::{self, Expression};
use crate::paginate::PaginatedResponse;
use crate::pyfmt;
use crate::value::{Map, Value};

pub mod errors;
pub mod stdout;
pub mod table;
pub mod text;
mod width_table;
pub mod yaml;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Text,
    Table,
    Yaml,
    YamlStream,
    Off,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        Some(match s {
            "json" => Format::Json,
            "text" => Format::Text,
            "table" => Format::Table,
            "yaml" => Format::Yaml,
            "yaml-stream" => Format::YamlStream,
            "off" => Format::Off,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Auto,
    On,
    Off,
}

pub struct OutputOptions {
    pub format: Format,
    /// `--query`.
    pub query: Option<String>,
    pub color: Color,
    /// Width of the terminal attached to stdout; `None` when stdout is not
    /// a terminal (the table formatter then uses 80).
    pub terminal_width: Option<usize>,
    /// Whether stdout is a tty (`--color auto`).
    pub stdout_is_tty: bool,
}

pub enum Response {
    Single(Value),
    Paginated(PaginatedResponse),
}

/// Format `response` like the AWS CLI formatter for
/// `command_name` (the operation name, e.g. `DescribeInstances`, which the
/// table formatter uses as the title).
///
/// Errors carry the message of the Python exception the formatter would
/// raise (the caller reports them like `GeneralExceptionHandler`); output
/// the Python formatter writes before failing is written first.
pub fn write_response(
    command_name: &str,
    response: Response,
    options: &OutputOptions,
    out: &mut dyn Write,
) -> Result<()> {
    let query = options
        .query
        .as_deref()
        .map(Expression::compile)
        .transpose()?;
    let mut buf = String::new();
    let result = format_response(command_name, response, options, query.as_ref(), &mut buf);
    let written = out.write_all(buf.as_bytes()).and_then(|()| out.flush());
    // The fully buffered and YAML stream formatters swallow `OSError`
    // (e.g. a closed pipe); the text formatter lets it propagate.
    if options.format == Format::Text {
        written?;
    }
    result
}

fn format_response(
    command_name: &str,
    response: Response,
    options: &OutputOptions,
    query: Option<&Expression>,
    buf: &mut String,
) -> Result<()> {
    match options.format {
        Format::Json => {
            let data = transformed(full(response), query)?;
            if !is_empty_dict(&data) {
                buf.push_str(&pyfmt::json_dumps_pretty(&data));
                buf.push('\n');
            }
        }
        Format::Yaml => {
            let data = transformed(full(response), query)?;
            if !is_empty_dict(&data) {
                yaml::dump(&data, buf);
            }
        }
        Format::YamlStream => match response {
            Response::Paginated(p) => {
                for page in p.pages {
                    let page = transformed(page, query)?;
                    yaml::dump(&Value::List(vec![page]), buf);
                }
            }
            Response::Single(v) => {
                let data = transformed(v, query)?;
                if !is_empty_dict(&data) {
                    yaml::dump(&Value::List(vec![data]), buf);
                }
            }
        },
        Format::Table => {
            let data = transformed(full(response), query)?;
            let (styler, autoreset) = table_styler(options.color, options.stdout_is_tty);
            let width = options.terminal_width.unwrap_or(80);
            render_table(command_name, &data, styler, autoreset, width, buf)?;
            if styler == table::Styler::Colorized && options.stdout_is_tty {
                // colorama's atexit `reset_all` on a tty stdout.
                buf.push_str("\x1b[0m");
            }
        }
        Format::Text => format_text_response(response, query, buf)?,
        Format::Off => {}
    }
    Ok(())
}

fn full(response: Response) -> Value {
    match response {
        Response::Single(v) => v,
        Response::Paginated(p) => p.full_result,
    }
}

fn is_empty_dict(v: &Value) -> bool {
    matches!(v, Value::Map(m) if m.is_empty())
}

/// `Formatter._remove_request_id`, including the `TypeError`s Python's
/// `'ResponseMetadata' in response` and `del` raise for non-dict values.
fn remove_request_id(v: &mut Value) -> Result<()> {
    const KEY: &str = "ResponseMetadata";
    match v {
        Value::Map(m) => {
            m.shift_remove(KEY);
            Ok(())
        }
        Value::Str(s) if s.contains(KEY) => {
            Err(anyhow!("'str' object doesn't support item deletion"))
        }
        Value::List(l) if l.iter().any(|x| x.as_str() == Some(KEY)) => {
            Err(anyhow!("list indices must be integers or slices, not str"))
        }
        Value::Str(_) | Value::List(_) => Ok(()),
        Value::Bytes(_) => Err(anyhow!("a bytes-like object is required, not 'str'")),
        other => Err(anyhow!(
            "argument of type '{}' is not a container or iterable",
            text::type_name(other)
        )),
    }
}

/// `Formatter._get_transformed_response_for_output`.
fn transformed(mut v: Value, query: Option<&Expression>) -> Result<Value> {
    remove_request_id(&mut v)?;
    match query {
        Some(q) => q.search(&v),
        None => Ok(v),
    }
}

/// The styler `TableFormatter.__init__` picks, and whether the stream is
/// colorama's autoreset wrapper (`colorama.init` replaces `sys.stdout`
/// when `ColorizedStyler` is created).
fn table_styler(color: Color, stdout_is_tty: bool) -> (table::Styler, bool) {
    let colorized = match color {
        Color::Auto => stdout_is_tty,
        Color::On => true,
        Color::Off => false,
    };
    if colorized {
        (table::Styler::Colorized, true)
    } else {
        (table::Styler::Plain, false)
    }
}

/// `TableFormatter._format_response`.
pub(crate) fn render_table(
    title: &str,
    data: &Value,
    styler: table::Styler,
    autoreset: bool,
    terminal_width: usize,
    buf: &mut String,
) -> Result<()> {
    let mut builder = table::TableBuilder {
        table: table::MultiTable::new(styler, terminal_width),
    };
    if builder
        .build_table(Some(title), data, 0)
        .map_err(|e| anyhow!(e))?
    {
        let mut stream = table::Stream {
            out: buf,
            autoreset,
        };
        builder.table.render(&mut stream).map_err(|e| anyhow!(e))?;
    }
    Ok(())
}

/// `TextFormatter.__call__`.
fn format_text_response(
    response: Response,
    query: Option<&Expression>,
    buf: &mut String,
) -> Result<()> {
    match response {
        Response::Paginated(p) => {
            let result_keys: Vec<Expression> = p
                .result_keys
                .iter()
                .map(|k| Expression::compile(k))
                .collect::<Result<_>>()?;
            for (i, page) in p.pages.iter().enumerate() {
                let mut current: Map = if i > 0 {
                    Map::new()
                } else {
                    p.non_aggregate_part.clone()
                };
                for result_key in &result_keys {
                    let data = result_key.search(page)?;
                    jmes::set_value_from_jmespath(&mut current, result_key.source(), data);
                }
                text_format(Value::Map(current), query, buf)?;
            }
            if let Some(token) = &p.resume_token {
                let mut inner = Map::new();
                inner.insert("NextToken".to_string(), Value::str(token.clone()));
                let mut outer = Map::new();
                outer.insert("NextToken".to_string(), Value::Map(inner));
                text_format(Value::Map(outer), query, buf)?;
            }
        }
        Response::Single(mut v) => {
            remove_request_id(&mut v)?;
            text_format(v, query, buf)?;
        }
    }
    Ok(())
}

/// `TextFormatter._format_response`.
fn text_format(v: Value, query: Option<&Expression>, buf: &mut String) -> Result<()> {
    let v = match query {
        Some(q) => q.search(&v)?,
        None => v,
    };
    text::format_text(&v, buf).map_err(|e| anyhow!(e))
}

#[cfg(test)]
pub(crate) mod goldens;

#[cfg(test)]
pub(crate) mod test_support {
    use base64::Engine;

    use crate::value::Value;

    /// Decode a golden input: JSON with bytes as `{"$bytes": "<base64>"}`.
    pub(crate) fn decode(json: &str) -> Value {
        restore_bytes(Value::parse_json(json).expect("golden JSON"))
    }

    fn restore_bytes(v: Value) -> Value {
        match v {
            Value::Map(m) => {
                if m.len() == 1
                    && let Some(Value::Str(b)) = m.get("$bytes")
                {
                    return Value::Bytes(
                        base64::engine::general_purpose::STANDARD
                            .decode(b)
                            .expect("golden base64"),
                    );
                }
                Value::Map(m.into_iter().map(|(k, v)| (k, restore_bytes(v))).collect())
            }
            Value::List(l) => Value::List(l.into_iter().map(restore_bytes).collect()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::decode;
    use super::*;

    fn color(s: &str) -> Color {
        match s {
            "on" => Color::On,
            "off" => Color::Off,
            _ => Color::Auto,
        }
    }

    #[test]
    fn formatter_goldens() {
        check_format_cases(goldens::FORMAT);
    }

    /// Cases that run JMESPath through `crate::jmes`.
    #[test]
    fn formatter_jmespath_goldens() {
        check_format_cases(goldens::FORMAT_JMES);
    }

    fn check_format_cases(cases: &[goldens::FormatCase]) {
        let mut failures = Vec::new();
        for &(name, fmt, command, query, col, tty, width, spec, single, expected, error) in cases {
            let response = match (spec, single) {
                (Some(spec), _) => {
                    let spec = decode(spec);
                    let get = |k: &str| spec.get(k).cloned().unwrap_or_default();
                    Response::Paginated(PaginatedResponse {
                        pages: get("pages").as_list().cloned().unwrap_or_default(),
                        result_keys: get("result_keys")
                            .as_list()
                            .map(|l| {
                                l.iter()
                                    .map(|v| v.as_str().unwrap_or_default().to_string())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        non_aggregate_part: get("non_aggregate_part")
                            .as_map()
                            .cloned()
                            .unwrap_or_default(),
                        resume_token: get("resume_token").as_str().map(str::to_string),
                        full_result: get("full_result"),
                    })
                }
                (None, Some(single)) => Response::Single(decode(single)),
                (None, None) => unreachable!(),
            };
            let options = OutputOptions {
                format: Format::parse(fmt).expect("format"),
                query: query.map(str::to_string),
                color: color(col),
                terminal_width: Some(width),
                stdout_is_tty: tty,
            };
            let mut out = Vec::new();
            let result = write_response(command, response, &options, &mut out);
            let actual = String::from_utf8(out).expect("UTF-8 output");
            let actual_error = result.err().map(|e| e.to_string());
            if actual != expected || actual_error.as_deref() != error {
                failures.push(format!(
                    "{name}\n--- expected ({error:?})\n{expected}\n--- actual ({actual_error:?})\n{actual}"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} cases differ:\n{}",
            failures.len(),
            cases.len(),
            failures.join("\n")
        );
    }
}
