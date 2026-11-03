//! `events wizard` (`awscli/customizations/wizard/commands.py`).

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "wizard" => super::dynamodb::wizard(ctx, "events", args),
        _ => Err(crate::commands::not_implemented()),
    }
}
