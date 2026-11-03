//! `translate import-terminology --data-file` and
//! `translate-document --document-content`
//! (`awscli/customizations/translate.py`, `binaryhoist.py`
//! `BinaryBlobArgumentHoister`).

use super::exec;
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    let mut hooks = match name {
        "import-terminology" => BlobHoister {
            new_name: "data-file",
            original: "terminology-data",
            serialized: "TerminologyData",
            member: "File",
            original_required: false,
            error: "File cannot be provided as part of the '--terminology-data' argument. Please use the '--data-file' option instead to specify a file.",
        },
        "translate-document" => BlobHoister {
            new_name: "document-content",
            original: "document",
            serialized: "Document",
            member: "Content",
            original_required: true,
            error: "Content cannot be provided as a part of the '--document' argument. Please use the '--document-content' option instead to specify a file.",
        },
        _ => return Err(crate::commands::not_implemented()),
    };
    exec::run_operation(ctx, "translate", name, args, &mut hooks)
}

/// `BinaryBlobArgumentHoister`: a required blob argument for one member
/// of a structure argument, which may no longer carry that member.
struct BlobHoister {
    new_name: &'static str,
    original: &'static str,
    serialized: &'static str,
    member: &'static str,
    original_required: bool,
    error: &'static str,
}

impl OperationHooks for BlobHoister {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        table.insert(
            self.new_name.to_string(),
            Arg::custom_argument(self.new_name, CustomSpec::default(), true),
        );
        if let Some(a) = table.get_mut(self.original) {
            a.required = self.original_required;
        }
    }

    /// `InjectingArgument.add_to_params` (which also sets the member when
    /// the value is `None`), after `OriginalArgument.add_to_params` has
    /// rejected the member in the original argument.
    fn add_to_params(
        &mut self,
        _name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        if let Some(Value::Map(original)) = params.get(self.serialized)
            && original.contains_key(self.member)
        {
            return Err(Fail::ParamValidation(self.error.to_string()));
        }
        let value = value.unwrap_or(Value::Null);
        match params.get_mut(self.serialized) {
            Some(Value::Map(m)) if !m.is_empty() => {
                m.insert(self.member.to_string(), value);
            }
            _ => {
                let mut m = Map::new();
                m.insert(self.member.to_string(), value);
                params.insert(self.serialized.to_string(), Value::Map(m));
            }
        }
        Ok(())
    }
}
