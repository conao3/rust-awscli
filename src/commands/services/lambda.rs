//! `lambda` `--zip-file` arguments (`awscli/customizations/awslambda.py`):
//! `create-function` / `publish-layer-version` hoist `ZipFile` out of
//! `--code` / `--content`, and `update-function-code` checks that
//! `--zip-file` is a zip archive.

use serde_json::{Map as JsonMap, Value as Json};

use crate::bjson::{Node, Obj};

use super::exec;
use crate::cli::argprocess::{ArgContext, unpack_argument};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::{Operation, ServiceModel, Shape};
use crate::value::{Map, Value};

const ERROR_MSG: &str = "--zip-file must be a zip file with the fileb:// prefix.\nExample usage:  --zip-file fileb://path/to/file.zip";

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    if name == "wizard" {
        return super::wizard::run(ctx, "lambda", args);
    }
    let mut hooks = match name {
        "create-function" => ZipFile::hoist("Code", name),
        "publish-layer-version" => ZipFile::hoist("Content", name),
        "update-function-code" => ZipFile {
            serialized: "ZipFile",
            operation: "update-function-code",
            replaced: None,
        },
        _ => return Err(crate::commands::not_implemented()),
    };
    exec::run_operation(ctx, "lambda", name, args, &mut hooks)
}

/// A copy of `shape` (named `InputShape`) without `member`, with the
/// shapes it references (`copy.deepcopy(argument_model)` +
/// `del model.members[member]`).
pub(crate) fn shape_without_member(shape: &Shape<'_>, member: &str) -> ServiceModel {
    fn refs<'d>(def: &'d Obj, skip: Option<&str>) -> Vec<&'d str> {
        let mut refs: Vec<&str> = Vec::new();
        if let Some(members) = def.get("members").and_then(Node::as_object) {
            refs.extend(
                members
                    .iter()
                    .filter(|(k, _)| Some(*k) != skip)
                    .filter_map(|(_, r)| r.get("shape")?.as_str()),
            );
        }
        for key in ["member", "key", "value"] {
            if let Some(r) = def
                .get(key)
                .and_then(|r| r.get("shape"))
                .and_then(Node::as_str)
            {
                refs.push(r);
            }
        }
        refs
    }
    fn collect(model: &ServiceModel, names: Vec<&str>, out: &mut JsonMap<String, Json>) {
        for name in names {
            if out.contains_key(name) {
                continue;
            }
            if let Some(d) = model.shape_def(name) {
                out.insert(name.to_string(), d.to_json());
                collect(model, refs(d, None), out);
            }
        }
    }
    let mut def = shape.def.to_map();
    if let Some(Json::Object(members)) = def.get_mut("members") {
        members.shift_remove(member);
    }
    if let Some(Json::Array(required)) = def.get_mut("required") {
        required.retain(|r| r.as_str() != Some(member));
    }
    let mut shapes = JsonMap::new();
    collect(shape.model, refs(shape.def, Some(member)), &mut shapes);
    shapes.insert("InputShape".to_string(), Json::Object(def));
    ServiceModel::from_shapes(&shape.model.name, Json::Object(shapes))
}

/// `zipfile.ZipFile(BytesIO(value)).infolist()` succeeds: the end of
/// central directory record is found and the central directory entries
/// it points at carry their signature.
fn is_zip(data: &[u8]) -> bool {
    const EOCD: &[u8] = b"PK\x05\x06";
    if data.len() < 22 {
        return false;
    }
    let start = data.len().saturating_sub(22 + 65535);
    let Some(pos) = (start..=data.len() - 22)
        .rev()
        .find(|&i| &data[i..i + 4] == EOCD)
    else {
        return false;
    };
    let le32 =
        |i: usize| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
    let size_cd = le32(pos + 12);
    let offset_cd = le32(pos + 16);
    let Some(concat) = pos.checked_sub(size_cd + offset_cd) else {
        return false;
    };
    let mut p = offset_cd + concat;
    let end = p + size_cd;
    while p < end {
        if p + 46 > data.len() || &data[p..p + 4] != b"PK\x01\x02" {
            return false;
        }
        let le16 = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]) as usize;
        p += 46 + le16(p + 28) + le16(p + 30) + le16(p + 32);
    }
    true
}

fn bytes_of(value: &Value) -> Vec<u8> {
    match value {
        Value::Bytes(b) => b.clone(),
        Value::Str(s) => s.as_bytes().to_vec(),
        other => crate::pyfmt::py_str(other).into_bytes(),
    }
}

struct ZipFile {
    /// The parameter `--zip-file` goes to (`Code`, `Content`, or the
    /// top-level `ZipFile`).
    serialized: &'static str,
    operation: &'static str,
    /// The model of the replaced argument (`--code` / `--content`
    /// without `ZipFile`).
    replaced: Option<ServiceModel>,
}

impl ZipFile {
    fn hoist(serialized: &'static str, operation: &str) -> ZipFile {
        ZipFile {
            serialized,
            operation: if operation == "create-function" {
                "create-function"
            } else {
                "publish-layer-version"
            },
            replaced: None,
        }
    }

    fn replaced_name(&self) -> String {
        self.serialized.to_lowercase()
    }
}

impl OperationHooks for ZipFile {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        if self.serialized == "ZipFile" {
            if let Some(arg) = table.get("zip-file") {
                let required = arg.required;
                table.insert(
                    "zip-file".to_string(),
                    Arg::custom_argument("zip-file", CustomSpec::default(), required),
                );
            }
            return;
        }
        table.insert(
            "zip-file".to_string(),
            Arg::custom_argument("zip-file", CustomSpec::default(), false),
        );
        let name = self.replaced_name();
        if let Some(arg) = table.get(&name)
            && let Some(shape) = &arg.shape
        {
            self.replaced = Some(shape_without_member(shape, "ZipFile"));
            table.insert(
                name.clone(),
                Arg::custom_argument(&name, CustomSpec::default(), false),
            );
        }
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        if name == "zip-file" {
            if !is_zip(&bytes_of(&value)) {
                return Err(Fail::ParamValidation(ERROR_MSG.to_string()));
            }
            if self.serialized == "ZipFile" {
                params.insert("ZipFile".to_string(), value);
                return Ok(());
            }
            merge(params, self.serialized, "ZipFile", value);
            return Ok(());
        }
        let model = self.replaced.as_ref().expect("replaced model");
        let shape = model.shape("InputShape").expect("InputShape");
        let cli_name = format!("--{name}");
        let unpacked = unpack_argument(
            &ArgContext {
                name,
                cli_name: &cli_name,
                shape: &shape,
                service: "lambda",
                operation: self.operation,
            },
            &value,
        )?;
        if let Value::Map(m) = &unpacked {
            if m.contains_key("ZipFile") {
                return Err(Fail::ParamValidation(format!(
                    "ZipFile cannot be provided as part of the {cli_name} argument.  Please use the '--zip-file' option instead to specify a zip file."
                )));
            }
            match params.get_mut(self.serialized) {
                Some(Value::Map(existing)) if !existing.is_empty() => {
                    for (k, v) in m {
                        existing.insert(k.clone(), v.clone());
                    }
                }
                _ => {
                    params.insert(self.serialized.to_string(), unpacked);
                }
            }
        } else {
            params.insert(self.serialized.to_string(), unpacked);
        }
        Ok(())
    }
}

/// `parameters[param].update({key: value})` or `parameters[param] = {key: value}`.
fn merge(params: &mut Map, param: &str, key: &str, value: Value) {
    match params.get_mut(param) {
        Some(Value::Map(existing)) if !existing.is_empty() => {
            existing.insert(key.to_string(), value);
        }
        _ => {
            let mut m = Map::new();
            m.insert(key.to_string(), value);
            params.insert(param.to_string(), Value::Map(m));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_detection() {
        let empty_zip = b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
        assert!(is_zip(empty_zip));
        assert!(!is_zip(b"not a zip file at all, really not"));
    }
}
