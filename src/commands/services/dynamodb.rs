//! `dynamodb wizard` and the `wizard` commands of the other services
//! (`awscli/customizations/wizard/commands.py`).

use std::io::IsTerminal as _;

use super::basic;
use crate::cli::argparse::{Action, Nargs, NsVal, Parsed, Parser};
use crate::cli::render::argparse_error;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "wizard" => wizard(ctx, "dynamodb", args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `WizardLoader.list_available_wizards(command)` (the files of
/// `awscli/customizations/wizard/wizards/<command>/`).
fn available_wizards(service: &str) -> &'static [&'static str] {
    match service {
        "configure" => &["_main"],
        "dynamodb" => &["new-table"],
        "events" => &["new-rule"],
        "iam" => &["new-role"],
        "lambda" => &["new-function"],
        _ => &[],
    }
}

/// `TopLevelWizardCommand` and its `SingleWizardCommand` subcommands.
pub(crate) fn wizard(ctx: &Context, service: &str, args: &[String]) -> Result<ExitCode, CliError> {
    let render = |f| crate::cli::render_fail(f, &ctx.globals, &ctx.session);
    let all = available_wizards(service);
    let names: Vec<&str> = all
        .iter()
        .copied()
        .filter(|n| !n.starts_with('_'))
        .collect();
    if args.len() == 1 && args[0] == "help" {
        let mut out = String::from("wizard\n\nAvailable commands:\n\n");
        for n in &names {
            out.push_str(&format!("  * {n}\n"));
        }
        crate::driver::write_stdout(ctx, out.as_bytes())?;
        return Ok(ExitCode::Success);
    }
    let mut parser = Parser::new();
    if !names.is_empty() {
        let mut sub = Action::positional("subcommand", Nargs::Optional);
        sub.choices = Some(names.iter().map(|s| s.to_string()).collect());
        parser.add(sub);
    }
    let (ns, remaining) = match parser
        .parse_known_args(args)
        .map_err(|e| render(argparse_error(&e.to_string())))?
    {
        Parsed::Args { ns, extras } => (ns, extras),
        Parsed::Version => unreachable!("no version action"),
    };
    let (name, rest) = match ns.get("subcommand") {
        Some(NsVal::Str(s)) => {
            let mut rest = args.to_vec();
            if let Some(i) = rest.iter().position(|a| a == s) {
                rest.remove(i);
            }
            if basic::parse_args(&[service, "wizard", s], &[], &rest)
                .map_err(render)?
                .is_none()
            {
                crate::driver::write_stdout(ctx, format!("{s}\n\nOptions:\n\n").as_bytes())?;
                return Ok(ExitCode::Success);
            }
            (s.clone(), rest)
        }
        _ => {
            if !remaining.is_empty() {
                return Err(render(crate::cli::Fail::ParamValidation(
                    crate::cli::format_unknown_options_error(
                        &remaining,
                        &crate::cli::option_suggestion_candidates(None),
                    ),
                )));
            }
            if !all.contains(&"_main") {
                return Err(basic::usage_error(ctx, &[service, "wizard"]));
            }
            ("_main".to_string(), Vec::new())
        }
    };
    let _ = (name, rest);
    ctx.session.set_command(service, "wizard");
    if !std::io::stdin().is_terminal() {
        basic::eprint("Warning: Input is not a terminal (fd=0).\n");
        return Err(CliError::new(
            ExitCode::General,
            crate::output::errors::render_general_error(""),
        ));
    }
    Err(crate::commands::not_implemented())
}
