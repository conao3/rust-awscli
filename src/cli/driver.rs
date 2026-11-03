//! Dispatching a parsed command line (`CLIDriver.main`,
//! `ServiceCommand.__call__`, `ServiceOperation.__call__`,
//! `waiters.WaitCommand`, `awscli/customizations/commands.py`
//! `BasicCommand.__call__`).

use super::argparse::{Action, Nargs, NsVal, Parsed, Parser};
use super::arguments::CommandPath;
use super::commands::{
    MainCommand, ServiceEntry, find_main, main_commands, service_commands, waiter_commands,
};
use super::operation::{self, Invocation};
use super::render::argparse_error;
use super::{Command, Ctx, Fail, WaitInvocation};
use crate::model::ServiceModel;

pub(crate) fn dispatch(rest: &[String], ctx: &Ctx<'_>) -> Result<Command, Fail> {
    if ctx.globals.version {
        return Ok(Command::Version);
    }
    let Some((command, remaining)) = rest.split_first() else {
        return Err(argparse_error(
            "the following arguments are required: command",
        ));
    };
    session_initialized(ctx)?;
    if command == "help" {
        return Ok(Command::Print(provider_help()));
    }
    match find_main(command) {
        Some(MainCommand::Service(cli, service)) => service_command(cli, service, remaining, ctx),
        Some(MainCommand::Custom(name)) => Ok(Command::Custom {
            path: vec![name.to_string()],
            args: remaining.to_vec(),
        }),
        None => Err(argparse_error(&format!(
            "argument command: Found invalid choice '{command}'\n"
        ))),
    }
}

/// The `session-initialized` handlers that can fail
/// (`binaryformat.add_binary_formatter`).
fn session_initialized(ctx: &Ctx<'_>) -> Result<(), Fail> {
    let format = binary_format(ctx);
    if !matches!(format.as_str(), "base64" | "raw-in-base64-out") {
        return Err(Fail::General(crate::pyfmt::str_repr(&format)));
    }
    Ok(())
}

/// `cli_binary_format` from the command line or the configuration.
pub(crate) fn binary_format(ctx: &Ctx<'_>) -> String {
    ctx.globals
        .cli_binary_format
        .clone()
        .or_else(|| ctx.config.config_variable("cli_binary_format"))
        .unwrap_or_else(|| "base64".to_string())
}

fn provider_help() -> String {
    let mut out = String::from("aws\n\nAvailable services:\n\n");
    let mut names: Vec<&str> = main_commands()
        .map(|c| match c {
            MainCommand::Service(n, _) | MainCommand::Custom(n) => n,
        })
        .collect();
    names.sort();
    for n in names {
        out.push_str(&format!("  * {n}\n"));
    }
    out
}

/// `ServiceCommand.__call__`.
fn service_command(
    cli: &str,
    service: &str,
    args: &[String],
    ctx: &Ctx<'_>,
) -> Result<Command, Fail> {
    let model = ServiceModel::load(service).map_err(|e| Fail::General(e.to_string()))?;
    let table = service_commands(cli, &model);
    let mut choices: Vec<String> = table.iter().map(|(n, _)| n.clone()).collect();
    choices.push("help".to_string());
    let mut parser = Parser::new();
    let mut operation = Action::positional("operation", Nargs::One);
    operation.choices = Some(choices);
    parser.add(operation);
    let (ns, remaining) = match parser
        .parse_known_args(args)
        .map_err(|e| argparse_error(&e.to_string()))?
    {
        Parsed::Args { ns, extras } => (ns, extras),
        Parsed::Version => unreachable!("no version action"),
    };
    let NsVal::Str(name) = &ns["operation"] else {
        unreachable!("required positional")
    };
    if name == "help" {
        return Ok(Command::Print(service_help(cli, &table)));
    }
    let entry = table
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, e)| e.clone())
        .expect("choice");
    match entry {
        ServiceEntry::Operation(op) => {
            if super::commands::is_customized(cli, name) {
                return Ok(Command::Custom {
                    path: vec![cli.to_string(), name.to_string()],
                    args: remaining.to_vec(),
                });
            }
            let op = model.operation(&op).expect("operation");
            let path = CommandPath { parent: cli, name };
            match operation::run(&op, path, &remaining, ctx)? {
                Invocation::Print(text) => Ok(Command::Print(text)),
                Invocation::Call(mut inv) => {
                    inv.cli_service = cli.to_string();
                    Ok(Command::Operation(*inv))
                }
            }
        }
        ServiceEntry::Wait => wait_command(cli, &model, &remaining, ctx),
        ServiceEntry::Custom => Ok(Command::Custom {
            path: vec![cli.to_string(), name.to_string()],
            args: remaining.to_vec(),
        }),
    }
}

/// The operation `name` of the `cli` service table parsed with a
/// customization's handlers (`commands::services` calls this for the
/// operations of `super::commands::is_customized`).
pub(crate) fn customized_operation(
    cli: &str,
    name: &str,
    args: &[String],
    ctx: &Ctx<'_>,
    hooks: &mut dyn super::OperationHooks,
) -> Result<Command, Fail> {
    let service = match find_main(cli) {
        Some(MainCommand::Service(_, service)) => service,
        _ => return Err(Fail::General(format!("unknown service command {cli}"))),
    };
    let model = ServiceModel::load(service).map_err(|e| Fail::General(e.to_string()))?;
    let Some(ServiceEntry::Operation(op)) = service_commands(cli, &model)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, e)| e)
    else {
        return Err(Fail::General(format!("unknown operation {cli} {name}")));
    };
    let op = model.operation(&op).expect("operation");
    let path = CommandPath { parent: cli, name };
    match operation::run_with(&op, path, args, ctx, hooks)? {
        Invocation::Print(text) => Ok(Command::Print(text)),
        Invocation::Call(mut inv) => {
            inv.cli_service = cli.to_string();
            Ok(Command::Operation(*inv))
        }
    }
}

fn service_help(cli: &str, table: &[(String, ServiceEntry)]) -> String {
    let mut out = format!("{cli}\n\nAvailable commands:\n\n");
    for (n, _) in table {
        out.push_str(&format!("  * {n}\n"));
    }
    out
}

/// `WaitCommand` (`BasicCommand.__call__` with the waiter subcommands).
fn wait_command(
    cli: &str,
    model: &ServiceModel,
    args: &[String],
    ctx: &Ctx<'_>,
) -> Result<Command, Fail> {
    let waiters = waiter_commands(model);
    if args.len() == 1 && args[0] == "help" {
        let mut out = format!("{cli} wait\n\nAvailable commands:\n\n");
        for (n, _) in &waiters {
            out.push_str(&format!("  * {n}\n"));
        }
        return Ok(Command::Print(out));
    }
    let mut parser = Parser::new();
    let mut sub = Action::positional("subcommand", Nargs::Optional);
    sub.choices = Some(waiters.iter().map(|(n, _)| n.clone()).collect());
    parser.add(sub);
    let (ns, remaining) = match parser
        .parse_known_args(args)
        .map_err(|e| argparse_error(&e.to_string()))?
    {
        Parsed::Args { ns, extras } => (ns, extras),
        Parsed::Version => unreachable!("no version action"),
    };
    let NsVal::Str(sub) = &ns["subcommand"] else {
        if !remaining.is_empty() {
            return Err(Fail::ParamValidation(
                operation::format_unknown_options_error(
                    &remaining,
                    &operation::option_suggestion_candidates(None),
                ),
            ));
        }
        return Err(Fail::ParamValidation(format!(
            "usage: aws [options] {cli} wait <subcommand> [parameters]\naws: [ERROR]: too few arguments"
        )));
    };
    let mut new_args = args.to_vec();
    if let Some(p) = new_args.iter().position(|a| a == sub) {
        new_args.remove(p);
    }
    let waiter = waiters
        .iter()
        .find(|(n, _)| n == sub)
        .map(|(_, w)| w.clone())
        .expect("choice");
    let op_name = model.waiters().expect("waiters")[&waiter]["operation"]
        .as_str()
        .expect("waiter operation")
        .to_string();
    let op = model.operation(&op_name).expect("operation");
    let path = CommandPath {
        parent: "wait",
        name: sub,
    };
    match operation::run(&op, path, &new_args, ctx)? {
        Invocation::Print(text) => Ok(Command::Print(text)),
        Invocation::Call(inv) => Ok(Command::Wait(WaitInvocation {
            cli_service: cli.to_string(),
            service: model.name.clone(),
            waiter,
            params: inv.params,
        })),
    }
}
