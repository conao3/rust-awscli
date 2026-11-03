//! `configservice put-configuration-recorder --recording-group`
//! (`awscli/customizations/configservice/putconfigurationrecorder.py`).

pub(crate) mod cmds;

use super::exec;
use crate::cli::argprocess::{ArgContext, unpack_argument};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::{Operation, ServiceModel};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "subscribe" | "get-status" => cmds::run(ctx, name, args),
        "put-configuration-recorder" => {
            let model = ServiceModel::load("config").map_err(|e| ctx.general_error(&e))?;
            exec::run_operation(
                ctx,
                "configservice",
                name,
                args,
                &mut RecordingGroup { model },
            )
        }
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `extract_recording_group` and `RecordingGroupArgument`.
struct RecordingGroup {
    model: ServiceModel,
}

impl OperationHooks for RecordingGroup {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        if let Some(a) = table.get_mut("configuration-recorder") {
            a.required = true;
        }
        table.insert(
            "recording-group".to_string(),
            Arg::custom_argument("recording-group", CustomSpec::default(), false),
        );
    }

    fn add_to_params(
        &mut self,
        _name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        let shape = self
            .model
            .operation("PutConfigurationRecorder")
            .and_then(|op| op.input())
            .and_then(|i| i.member_shape("ConfigurationRecorder"))
            .and_then(|r| r.member_shape("recordingGroup"))
            .expect("recordingGroup shape");
        let unpacked = unpack_argument(
            &ArgContext {
                name: "recording-group",
                cli_name: "--recording-group",
                shape: &shape,
                service: "config",
                operation: "put-configuration-recorder",
            },
            &value,
        )?;
        let recorder = params
            .entry("ConfigurationRecorder".to_string())
            .or_insert_with(Value::map);
        if let Value::Map(m) = recorder {
            m.insert("recordingGroup".to_string(), unpacked);
        }
        Ok(())
    }
}
