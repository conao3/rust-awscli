//! `logs tail` and `logs start-live-tail` (`awscli/customizations/logs/`).

mod tail;

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "tail" => tail::tail(ctx, args),
        "start-live-tail" => tail::start_live_tail(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}
