//! `kinesis list-streams` pagination back-compat
//! (`awscli/customizations/kinesis.py`).

use indexmap::IndexMap;

use super::exec;
use crate::cli::argparse::NsVal;
use crate::cli::{ArgTable, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "list-streams" => exec::run_operation(ctx, "kinesis", name, args, &mut ListStreams),
        _ => Err(crate::commands::not_implemented()),
    }
}

struct ListStreams;

impl OperationHooks for ListStreams {
    /// `undocument_exclusive_start_stream_name`.
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        if let Some(a) = table.get_mut("exclusive-start-stream-name") {
            a.undocumented = true;
        }
    }

    /// `disable_pagination_when_exclusive_start_stream_name_provided`.
    fn args_parsed(
        &mut self,
        ns: &IndexMap<String, NsVal>,
        paginate: &mut bool,
    ) -> Result<(), Fail> {
        if ns
            .get("exclusive_start_stream_name")
            .is_some_and(|v| !v.is_none())
        {
            *paginate = false;
        }
        Ok(())
    }
}
