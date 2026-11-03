//! `rekognition` image arguments (`awscli/customizations/rekognition.py`,
//! `awscli/customizations/arguments.py` `NestedBlobArgumentHoister`):
//! `--image-bytes` (and `--source-image-bytes` / `--target-image-bytes` of
//! `compare-faces`) fill the `Bytes` member of the image structure.

use super::exec;
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    exec::run_operation(
        ctx,
        "rekognition",
        name,
        args,
        &mut Hoister { added: Vec::new() },
    )
}

/// `FILE_PARAMETER_UPDATES`: (operation or `*`, source argument, new argument).
const UPDATES: &[(&str, &str, &str)] = &[
    ("compare-faces", "source-image", "source-image-bytes"),
    ("compare-faces", "target-image", "target-image-bytes"),
    ("*", "image", "image-bytes"),
];

/// `_reverse_xform_name`.
fn reverse_xform_name(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut c = part.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect()
}

struct Hoister {
    /// (new argument, parameter to overwrite).
    added: Vec<(String, String)>,
}

impl OperationHooks for Hoister {
    fn table<'m>(&mut self, op: &Operation<'m>, table: &mut ArgTable<'m>) {
        let cli = crate::cli::arguments::xform_name(op.name, '-');
        for (operation, source, new) in UPDATES {
            if *operation != "*" && *operation != cli {
                continue;
            }
            let valid = table
                .get(*source)
                .and_then(|a| a.shape.as_ref())
                .is_some_and(|s| {
                    s.member_shape("Bytes")
                        .is_some_and(|m| m.type_name() == "blob")
                });
            if !valid {
                continue;
            }
            table.insert(
                new.to_string(),
                Arg::custom_argument(new, CustomSpec::default(), false),
            );
            if let Some(a) = table.get_mut(*source) {
                a.required = false;
            }
            self.added
                .push((new.to_string(), reverse_xform_name(source)));
        }
    }

    /// `_NestedBlobArgumentParamOverwrite.add_to_params`.
    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        let Some((_, param)) = self.added.iter().find(|(n, _)| n == name) else {
            return Ok(());
        };
        match params.get_mut(param) {
            Some(Value::Map(existing)) if !existing.is_empty() => {
                existing.insert("Bytes".to_string(), value);
            }
            _ => {
                let mut m = Map::new();
                m.insert("Bytes".to_string(), value);
                params.insert(param.clone(), Value::Map(m));
            }
        }
        Ok(())
    }
}
