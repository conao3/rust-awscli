//! `iot create-keys-and-certificate` / `create-certificate-from-csr`
//! outfile arguments (`awscli/customizations/iot.py`, `QueryOutFileArgument`
//! of `awscli/customizations/arguments.py`).

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use super::ec2::custom;
use super::exec::{self, After};
use crate::cli::paramfile::{expanduser, expandvars};
use crate::cli::{ArgTable, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    let outfiles: &[(&str, &str)] = match name {
        "create-keys-and-certificate" => &[
            ("certificate-pem-outfile", "certificatePem"),
            ("public-key-outfile", "keyPair.PublicKey"),
            ("private-key-outfile", "keyPair.PrivateKey"),
        ],
        "create-certificate-from-csr" => &[("certificate-pem-outfile", "certificatePem")],
        _ => return Err(crate::commands::not_implemented()),
    };
    let mut hooks = QueryOutFiles {
        args: outfiles,
        saves: Vec::new(),
    };
    let Some(inv) = exec::parse_operation(ctx, "iot", name, args, &mut hooks)? else {
        return Ok(ExitCode::Success);
    };
    let saves = hooks.saves;
    exec::invoke(ctx, inv, &mut |parsed| {
        if !is_parsed_result_successful(parsed) {
            return Ok(After::Display);
        }
        for (query, path) in &saves {
            let mut current = Some(&*parsed);
            for part in query.split('.') {
                current = current.and_then(|v| v.get(part));
            }
            let contents = match current {
                None | Some(Value::Null) => String::new(),
                Some(Value::Str(s)) => s.clone(),
                Some(other) => crate::pyfmt::py_str(other),
            };
            write_private(path, contents.as_bytes(), 0o600)
                .map_err(|e| ctx.general_error(&anyhow::anyhow!(e)))?;
        }
        Ok(After::Display)
    })
}

/// `is_parsed_result_successful`.
pub(super) fn is_parsed_result_successful(parsed: &Value) -> bool {
    parsed
        .get("ResponseMetadata")
        .and_then(|m| m.get("HTTPStatusCode"))
        .and_then(Value::as_i64)
        .is_none_or(|c| c < 300)
}

/// `resolve_given_outfile_path`.
pub(super) fn resolve_given_outfile_path(path: &str) -> Result<String, Fail> {
    let outfile = expanduser(&expandvars(path));
    let abs = std::path::absolute(&outfile).unwrap_or_else(|_| outfile.clone().into());
    let dir = abs.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap_or_default();
    if unsafe { libc::access(c.as_ptr(), libc::W_OK) } != 0 {
        return Err(Fail::ParamValidation(format!(
            "Unable to write to file: {outfile}"
        )));
    }
    Ok(outfile)
}

/// `compat_open(path, 'w'/'wb', access_permissions=perm)` + `chmod`.
pub(super) fn write_private(path: &str, data: &[u8], perm: u32) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(perm)
        .open(path)?;
    f.write_all(data)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(perm))
}

struct QueryOutFiles {
    args: &'static [(&'static str, &'static str)],
    /// (query, path) of the given arguments, in table order.
    saves: Vec<(&'static str, String)>,
}

impl OperationHooks for QueryOutFiles {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        for (name, _) in self.args {
            table.insert(name.to_string(), custom(name));
        }
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        _params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        let Some((_, query)) = self.args.iter().find(|(n, _)| *n == name) else {
            return Ok(());
        };
        let path = resolve_given_outfile_path(&crate::pyfmt::py_str(&value))?;
        self.saves.push((query, path));
        Ok(())
    }
}
