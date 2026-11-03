//! `quicksight start-asset-bundle-import-job
//! --asset-bundle-import-source-bytes` (`awscli/customizations/quicksight.py`,
//! `NestedBlobArgumentHoister` of `awscli/customizations/arguments.py`).

use super::exec;
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "start-asset-bundle-import-job" => {
            exec::run_operation(ctx, "quicksight", name, args, &mut AssetBundleBytes)
        }
        _ => Err(crate::commands::not_implemented()),
    }
}

struct AssetBundleBytes;

const NEW_ARG: &str = "asset-bundle-import-source-bytes";

impl OperationHooks for AssetBundleBytes {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        let valid = table
            .get("asset-bundle-import-source")
            .and_then(|a| a.shape.as_ref())
            .and_then(|s| s.member_shape("Body"))
            .is_some_and(|b| b.type_name() == "blob");
        if !valid {
            return;
        }
        table.insert(
            NEW_ARG.to_string(),
            Arg::custom_argument(NEW_ARG, CustomSpec::default(), false),
        );
        if let Some(a) = table.get_mut("asset-bundle-import-source") {
            a.required = false;
        }
    }

    /// `_NestedBlobArgumentParamOverwrite.add_to_params`.
    fn add_to_params(
        &mut self,
        _name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        match params.get_mut("AssetBundleImportSource") {
            Some(Value::Map(m)) if !m.is_empty() => {
                m.insert("Body".to_string(), value);
            }
            _ => {
                let mut m = Map::new();
                m.insert("Body".to_string(), value);
                params.insert("AssetBundleImportSource".to_string(), Value::Map(m));
            }
        }
        Ok(())
    }
}
