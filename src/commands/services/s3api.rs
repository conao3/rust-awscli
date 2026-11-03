//! `s3api select-object-content` (`awscli/customizations/s3events.py`):
//! the `outfile` positional argument and the `after-call` handler that
//! writes the `Records` payloads of the event stream to it.

use std::io::Write as _;

use super::exec::{self, After};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::{Context, client_error};
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::output::errors::ClientErrorInfo;
use crate::protocol::eventstream;
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

/// `S3SelectStreamOutputArgument.save_file`: the `Records` payloads of
/// the events, written as the stream is iterated; an error event raises
/// `EventStreamError` after the records before it are written.
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
    let model = crate::model::ServiceModel::load("s3").map_err(|e| ctx.general_error(&e))?;
    let parser = eventstream::EventParser {
        model: &model,
        shape: model
            .shape("SelectObjectContentEventStream")
            .expect("SelectObjectContentEventStream"),
        protocol: eventstream::Protocol::Xml,
        options: Default::default(),
    };
    let mut buffer = eventstream::Buffer::new();
    buffer.add_data(&stream);
    loop {
        let message = match buffer.next_message() {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) => return Err(ctx.general_error(&anyhow::anyhow!(e))),
        };
        let event = match parser.parse(&message) {
            Ok(Some(event)) => event,
            Ok(None) => continue,
            Err(eventstream::EventError::Stream(parsed)) => {
                let _ = file.flush();
                let field = |k: &str| {
                    parsed
                        .get("Error")
                        .and_then(|e| e.get(k))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                return Err(client_error(
                    ctx,
                    &ClientErrorInfo {
                        operation_name: "SelectObjectContent".to_string(),
                        code: field("Code"),
                        message: field("Message"),
                        max_retries_reached: None,
                        parsed,
                        modeled_fields: None,
                    },
                ));
            }
            Err(eventstream::EventError::Other(e)) => return Err(ctx.general_error(&e)),
        };
        if let Some(Value::Bytes(payload)) = event.get("Records").and_then(|r| r.get("Payload")) {
            file.write_all(payload)
                .map_err(|e| ctx.general_error(&anyhow::anyhow!(e.to_string())))?;
        }
    }
    map.shift_remove("Payload");
    Ok(())
}
