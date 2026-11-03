//! The `configservice` customizations.

pub(crate) mod cmds;

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "subscribe" | "get-status" => cmds::run(ctx, name, args),
        _ => Err(crate::commands::not_implemented()),
    }
}
