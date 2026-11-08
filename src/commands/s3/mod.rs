//! `aws s3`: the high-level S3 commands
//! (`awscli/customizations/s3/s3.py` `S3` and its subcommands).

pub(crate) mod args;
pub(crate) mod client;
mod engine;
mod results;
mod simple;
mod transfer;
pub(crate) mod utils;

use std::io::Write;

use crate::cli::Fail;
use crate::cli::argparse::{Action, Nargs, NsVal, Parsed, Parser};
use crate::cli::render::{argparse_error, render};
use crate::driver::{Context, client_error};
use crate::error::{CliError, ExitCode};
use client::CallError;

pub(crate) const SUBCOMMANDS: &[&str] = &[
    "ls", "website", "cp", "mv", "rm", "sync", "mb", "rb", "presign",
];

/// An exception out of a subcommand, by the handler that renders it.
#[derive(Debug)]
pub(crate) enum S3Fail {
    Fail(Fail),
    Call(CallError),
    Other(anyhow::Error),
}

impl From<Fail> for S3Fail {
    fn from(f: Fail) -> S3Fail {
        S3Fail::Fail(f)
    }
}

impl S3Fail {
    fn render(self, ctx: &Context) -> CliError {
        match self {
            S3Fail::Fail(f) => render(f, Some(&ctx.globals), Some(&ctx.session)),
            S3Fail::Call(CallError::Service(info)) => client_error(ctx, &info),
            S3Fail::Call(CallError::Validation(m)) => render(
                Fail::ParamValidation(m),
                Some(&ctx.globals),
                Some(&ctx.session),
            ),
            S3Fail::Call(CallError::Other(e)) | S3Fail::Other(e) => ctx.general_error(&e),
        }
    }
}

/// `uni_print(text)` to stdout, flushed.
pub(crate) fn out(bytes: &[u8]) {
    crate::output::stdout::write_flushed(bytes);
}

/// `uni_print(text, sys.stderr)`.
pub(crate) fn err(bytes: &[u8]) {
    let mut e = std::io::stderr().lock();
    let _ = e.write_all(bytes);
    let _ = e.flush();
}

/// `S3.__call__` (`BasicCommand.__call__` with `SUBCOMMANDS`).
pub(crate) fn run(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    match dispatch(ctx, args) {
        Ok(code) => Ok(code),
        Err(f) => Err(f.render(ctx)),
    }
}

fn dispatch(ctx: &Context, args: &[String]) -> Result<ExitCode, S3Fail> {
    // `SubCommandArgParser`: the (empty) argument table plus an optional
    // `subcommand` positional.
    if args.len() == 1 && args[0] == "help" {
        return Err(S3Fail::Fail(Fail::General(help_unavailable())));
    }
    let mut parser = Parser::new();
    let mut sub = Action::positional("subcommand", Nargs::Optional);
    sub.choices = Some(SUBCOMMANDS.iter().map(|s| s.to_string()).collect());
    parser.add(sub);
    let (ns, extras) = match parser
        .parse_known_args(args)
        .map_err(|e| argparse_error(&e.to_string()))?
    {
        Parsed::Args { ns, extras } => (ns, extras),
        Parsed::Version => unreachable!("no version action"),
    };
    if let NsVal::Str(name) = &ns["subcommand"] {
        let mut new_args = args.to_vec();
        if let Some(p) = new_args.iter().position(|a| a == name) {
            new_args.remove(p);
        }
        return run_subcommand(ctx, name, &new_args);
    }
    if !extras.is_empty() {
        let candidates = crate::cli::operation::option_suggestion_candidates(None);
        return Err(
            Fail::ParamValidation(crate::cli::operation::format_unknown_options_error(
                &extras,
                &candidates,
            ))
            .into(),
        );
    }
    Err(Fail::ParamValidation(
        "usage: awsr [options] s3 <subcommand> [parameters]\nawsr: [ERROR]: too few arguments"
            .into(),
    )
    .into())
}

fn help_unavailable() -> String {
    "Could not find executable named \"groff or mandoc\"".to_string()
}

/// Whether `register_sync_strategies` ran: it handles
/// `building-command-table.s3_sync`, which a `sync` an alias proxies to
/// does not emit.
pub(crate) fn sync_strategies(ctx: &Context) -> bool {
    ctx.session.lineage(&["s3", "sync"]).len() != 1
}

/// Run `aws s3 <name> <args>` (also used by `rb --force` for `rm`).
pub(crate) fn run_subcommand(
    ctx: &Context,
    name: &str,
    args: &[String],
) -> Result<ExitCode, S3Fail> {
    let parsed = match args::parse(name, args, sync_strategies(ctx))? {
        args::ParseOutcome::Help => return Err(Fail::General(help_unavailable()).into()),
        args::ParseOutcome::Args(a) => a,
    };
    ctx.session.set_command("s3", name);
    match name {
        "ls" => simple::ls(ctx, &parsed),
        "mb" => simple::mb(ctx, &parsed),
        "rb" => simple::rb(ctx, &parsed),
        "website" => simple::website(ctx, &parsed),
        "presign" => simple::presign(ctx, &parsed),
        _ => transfer::run(ctx, name, parsed),
    }
}
