//! `iam create-virtual-mfa-device --outfile --bootstrap-method`
//! (`awscli/customizations/iamvirtmfa.py`).

use super::exec::{self, After};
use super::iot::{is_parsed_result_successful, resolve_given_outfile_path, write_private};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::{Map, Value};

const CHOICES: [&str; 2] = ["QRCodePNG", "Base32StringSeed"];

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    if name == "wizard" {
        return super::wizard::run(ctx, "iam", args);
    }
    if name != "create-virtual-mfa-device" {
        return Err(crate::commands::not_implemented());
    }
    let mut hooks = VirtualMfa::default();
    let Some(inv) = exec::parse_operation(ctx, "iam", name, args, &mut hooks)? else {
        return Ok(ExitCode::Success);
    };
    exec::invoke(ctx, inv, &mut |parsed| {
        if !is_parsed_result_successful(parsed) {
            return Ok(After::Display);
        }
        let Some(Value::Map(device)) = parsed
            .as_map_mut()
            .and_then(|m| m.get_mut("VirtualMFADevice"))
        else {
            return Err(ctx.general_error(&anyhow::anyhow!("'VirtualMFADevice'")));
        };
        let Some(method) = &hooks.method else {
            return Ok(After::Display);
        };
        if let Some(body) = device.get(method.as_str()) {
            let bytes = match body {
                Value::Bytes(b) => b.clone(),
                other => crate::pyfmt::py_str(other).into_bytes(),
            };
            let decoded = crate::protocol::common::a2b_base64(&bytes)
                .ok()
                .ok_or_else(|| ctx.general_error(&anyhow::anyhow!("Incorrect padding")))?;
            let outfile = hooks.outfile.clone().unwrap_or_default();
            write_private(&outfile, &decoded, 0o600)
                .map_err(|e| ctx.general_error(&anyhow::anyhow!(e)))?;
            for choice in CHOICES {
                device.shift_remove(choice);
            }
        }
        Ok(After::Display)
    })
}

#[derive(Default)]
struct VirtualMfa {
    outfile: Option<String>,
    method: Option<String>,
}

impl OperationHooks for VirtualMfa {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        table.insert(
            "outfile".to_string(),
            Arg::custom_argument("outfile", CustomSpec::default(), true),
        );
        let spec = CustomSpec {
            choices: CHOICES.iter().map(|c| c.to_string()).collect(),
            ..CustomSpec::default()
        };
        table.insert(
            "bootstrap-method".to_string(),
            Arg::custom_argument("bootstrap-method", spec, true),
        );
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        _params: &mut Map,
    ) -> Result<(), Fail> {
        let text = value.as_ref().map(crate::pyfmt::py_str);
        match name {
            "outfile" => {
                self.outfile = text.map(|t| resolve_given_outfile_path(&t)).transpose()?;
            }
            "bootstrap-method" => self.method = text,
            _ => {}
        }
        Ok(())
    }
}
