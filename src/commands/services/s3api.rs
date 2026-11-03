//! `s3api select-object-content` (`awscli/customizations/s3events.py`):
//! the `outfile` positional argument and the `after-call` handler that
//! writes the `Records` payloads of the event stream to it
//! (`botocore/eventstream.py` decodes the stream).

use std::io::Write as _;

use super::exec::{self, After};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::{Context, client_error};
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::output::errors::ClientErrorInfo;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "select-object-content" => select_object_content(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

#[derive(Default)]
struct SelectHooks {
    outfile: Option<String>,
}

impl OperationHooks for SelectHooks {
    /// `add_event_stream_output_arg`.
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        table.insert(
            "outfile".to_string(),
            Arg::custom_argument(
                "outfile",
                CustomSpec {
                    positional: true,
                    ..CustomSpec::default()
                },
                false,
            ),
        );
    }

    /// `S3SelectStreamOutputArgument.add_to_params`.
    fn add_to_params(
        &mut self,
        _name: &str,
        value: Option<Value>,
        _params: &mut Map,
    ) -> Result<(), Fail> {
        self.outfile = value.map(|v| super::cloudwatch::value_str(&v));
        Ok(())
    }
}

fn select_object_content(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut hooks = SelectHooks::default();
    let Some(inv) = exec::parse_operation(ctx, "s3api", "select-object-content", args, &mut hooks)?
    else {
        return Ok(ExitCode::Success);
    };
    let outfile = hooks.outfile.clone();
    exec::invoke(ctx, inv, &mut |parsed| {
        save_file(ctx, outfile.as_deref(), parsed)?;
        Ok(After::Display)
    })
}

/// `S3SelectStreamOutputArgument.save_file`.
fn save_file(ctx: &Context, outfile: Option<&str>, parsed: &mut Value) -> Result<(), CliError> {
    let Some(map) = parsed.as_map_mut() else {
        return Ok(());
    };
    let Some(Value::Bytes(stream)) = map.get("Payload").cloned() else {
        return Ok(());
    };
    let path = outfile.unwrap_or_default();
    let mut file = std::fs::File::create(path).map_err(|e| {
        ctx.general_error(&anyhow::anyhow!(crate::cli::paramfile::os_error(&e, path)))
    })?;
    let mut rest = stream.as_slice();
    while !rest.is_empty() {
        let (message, consumed) = match decode_message(rest) {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) => return Err(ctx.general_error(&anyhow::anyhow!(e))),
        };
        rest = &rest[consumed..];
        let header = |name: &str| {
            message
                .headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        match header(":message-type").as_deref() {
            Some("error") | Some("exception") => {
                let (code, msg) = match header(":message-type").as_deref() {
                    Some("exception") => {
                        (header(":exception-type").unwrap_or_default(), String::new())
                    }
                    _ => (
                        header(":error-code").unwrap_or_default(),
                        header(":error-message").unwrap_or_default(),
                    ),
                };
                let mut error = Map::new();
                error.insert("Code".to_string(), Value::str(&code));
                error.insert("Message".to_string(), Value::str(&msg));
                let mut parsed = Map::new();
                parsed.insert("Error".to_string(), Value::Map(error));
                let _ = file.flush();
                return Err(client_error(
                    ctx,
                    &ClientErrorInfo {
                        operation_name: "SelectObjectContent".to_string(),
                        code: if code.is_empty() {
                            "Unknown".to_string()
                        } else {
                            code
                        },
                        message: if msg.is_empty() {
                            "Unknown".to_string()
                        } else {
                            msg
                        },
                        max_retries_reached: None,
                        parsed: Value::Map(parsed),
                        modeled_fields: None,
                    },
                ));
            }
            _ => {
                if header(":event-type").as_deref() == Some("Records") {
                    file.write_all(&message.payload)
                        .map_err(|e| ctx.general_error(&anyhow::anyhow!(e.to_string())))?;
                }
            }
        }
    }
    map.shift_remove("Payload");
    Ok(())
}

struct Message {
    /// String-valued headers (other header types are kept as their
    /// `str()`).
    headers: Vec<(String, String)>,
    payload: Vec<u8>,
}

/// One message of `EventStreamBuffer` (`botocore/eventstream.py`):
/// `None` when the data ends before the message does.
fn decode_message(data: &[u8]) -> Result<Option<(Message, usize)>, String> {
    if data.len() < 12 {
        return Ok(None);
    }
    let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let total = be32(&data[0..4]) as usize;
    let headers_len = be32(&data[4..8]) as usize;
    let prelude_crc = be32(&data[8..12]);
    if headers_len > 128 * 1024 {
        return Err(format!(
            "Header length of {headers_len} exceeded the maximum of {}",
            128 * 1024
        ));
    }
    if total > 16 * 1024 * 1024 {
        return Err(format!(
            "Total length of {total} exceeded the maximum of {}",
            16 * 1024 * 1024
        ));
    }
    let calculated = crc32fast::hash(&data[0..8]);
    if calculated != prelude_crc {
        return Err(format!(
            "Checksum mismatch: expected 0x{prelude_crc:08x}, calculated 0x{calculated:08x}"
        ));
    }
    if data.len() < total {
        return Ok(None);
    }
    if total < 16 + headers_len {
        return Err(format!("Invalid message length {total}"));
    }
    let message_crc = be32(&data[total - 4..total]);
    let calculated = crc32fast::hash(&data[..total - 4]);
    if calculated != message_crc {
        return Err(format!(
            "Checksum mismatch: expected 0x{message_crc:08x}, calculated 0x{calculated:08x}"
        ));
    }
    let mut headers = Vec::new();
    let mut h = &data[12..12 + headers_len];
    while !h.is_empty() {
        let n = h[0] as usize;
        let name = String::from_utf8_lossy(&h[1..1 + n]).into_owned();
        h = &h[1 + n..];
        let ty = h[0];
        h = &h[1..];
        let (value, used) = match ty {
            0 => ("True".to_string(), 0),
            1 => ("False".to_string(), 0),
            2 => ((h[0] as i8).to_string(), 1),
            3 => (i16::from_be_bytes([h[0], h[1]]).to_string(), 2),
            4 => (i32::from_be_bytes([h[0], h[1], h[2], h[3]]).to_string(), 4),
            5 | 8 => (
                i64::from_be_bytes(h[0..8].try_into().expect("8 bytes")).to_string(),
                8,
            ),
            6 | 7 => {
                let len = u16::from_be_bytes([h[0], h[1]]) as usize;
                (
                    String::from_utf8_lossy(&h[2..2 + len]).into_owned(),
                    2 + len,
                )
            }
            9 => (hex::encode(&h[0..16]), 16),
            _ => return Err(format!("Unknown header type {ty}")),
        };
        h = &h[used..];
        headers.push((name, value));
    }
    let payload = data[12 + headers_len..total - 4].to_vec();
    Ok(Some((Message { headers, payload }, total)))
}
