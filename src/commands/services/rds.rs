//! `rds add-option-to-option-group` / `remove-option-from-option-group`:
//! `ModifyOptionGroup` with `--options` (`awscli/customizations/rds.py`
//! `_rename_add_option`, `_rename_remove_option`).

use super::exec;
use crate::cli::{ArgTable, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "generate-db-auth-token" => super::presign::generate_db_auth_token(ctx, args),
        "add-option-to-option-group" => exec::run_operation(
            ctx,
            "rds",
            name,
            args,
            &mut RenameOptions {
                keep: "options-to-include",
                drop: "options-to-remove",
            },
        ),
        "remove-option-from-option-group" => exec::run_operation(
            ctx,
            "rds",
            name,
            args,
            &mut RenameOptions {
                keep: "options-to-remove",
                drop: "options-to-include",
            },
        ),
        _ => Err(crate::commands::not_implemented()),
    }
}

struct RenameOptions {
    keep: &'static str,
    drop: &'static str,
}

/// `utils.rename_argument` (the renamed entry moves to the end).
pub(super) fn rename_argument(table: &mut ArgTable<'_>, existing: &str, new_name: &str) {
    if let Some(mut arg) = table.shift_remove(existing) {
        arg.name = new_name.to_string();
        table.insert(new_name.to_string(), arg);
    }
}

impl OperationHooks for RenameOptions {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        rename_argument(table, self.keep, "options");
        table.shift_remove(self.drop);
    }
}
