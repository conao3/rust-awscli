//! `deploy` customizations (`awscli/customizations/codedeploy/`): the
//! revision location arguments of three operations, and the `push`,
//! `register`, `deregister`, `install`, and `uninstall` commands.

mod ops;

pub(crate) mod cmds;
pub(crate) mod zipfile;

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "create-deployment" | "get-application-revision" | "register-application-revision" => {
            ops::run(ctx, name, args)
        }
        "push" | "register" | "deregister" | "install" | "uninstall" => cmds::run(ctx, name, args),
        _ => Err(crate::commands::not_implemented()),
    }
}
