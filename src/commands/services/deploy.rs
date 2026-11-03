//! The `deploy` customizations.

pub(crate) mod cmds;
pub(crate) mod zipfile;

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "push" | "register" | "deregister" | "install" | "uninstall" => cmds::run(ctx, name, args),
        _ => Err(crate::commands::not_implemented()),
    }
}
