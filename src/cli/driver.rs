//! Dispatching a parsed command line (`CLIDriver.main`,
//! `ServiceCommand.__call__`, `ServiceOperation.__call__`,
//! `waiters.WaitCommand`, `awscli/customizations/commands.py`
//! `BasicCommand.__call__`).

use super::argparse::{Action, Nargs, NsVal, Parsed, Parser};
use super::arguments::CommandPath;
use super::commands::{MainCommand, ServiceEntry, find_main, service_commands, waiter_commands};
use super::operation::{self, Invocation};
use super::render::argparse_error;
use super::{Command, Ctx, Fail, WaitInvocation};
use crate::help::HelpRequest;
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
        return provider_help(remaining);
    }
    if !shadowed(ctx, &[command])
        && let Some(value) = super::alias::find(&[], command)?
    {
        return Ok(Command::Alias(super::alias::toplevel(
            command,
            &value,
            remaining,
            ctx.globals,
        )?));
    }
    match find_main(command) {
        Some(MainCommand::Service(cli, service)) => service_command(cli, service, remaining, ctx),
        Some(MainCommand::Custom(name)) => {
            if let Some(command) = custom_subcommand_alias(name, remaining, ctx)? {
                return Ok(command);
            }
            Ok(Command::Custom {
                path: vec![name.to_string()],
                args: remaining.to_vec(),
            })
        }
        None => Err(argparse_error(&format!(
            "argument command: Found invalid choice '{command}'\n"
        ))),
    }
}

/// `BasicCommand.__call__` of a command with subcommands when its
/// subcommand table has aliases (`AliasSubCommandInjector`): the alias
/// the `subcommand` positional names, if any.
fn custom_subcommand_alias(
    name: &str,
    args: &[String],
    ctx: &Ctx<'_>,
) -> Result<Option<Command>, Fail> {
    let subcommands: &[&str] = match name {
        "s3" => crate::commands::s3::SUBCOMMANDS,
        "configure" => crate::commands::configure::SUBCOMMANDS,
        _ => return Ok(None),
    };
    let aliases = super::alias::get(&[name])?;
    if aliases.is_empty() || (args.len() == 1 && args[0] == "help") {
        return Ok(None);
    }
    let mut choices: Vec<String> = subcommands.iter().map(|s| s.to_string()).collect();
    super::alias::extend_choices(&[name], &mut choices)?;
    let mut parser = Parser::new();
    let mut sub = Action::positional("subcommand", Nargs::Optional);
    sub.choices = Some(choices);
    parser.add(sub);
    let ns = match parser
        .parse_known_args(args)
        .map_err(|e| argparse_error(&e.to_string()))?
    {
        Parsed::Args { ns, .. } => ns,
        Parsed::Version => unreachable!("no version action"),
    };
    let NsVal::Str(sub) = &ns["subcommand"] else {
        return Ok(None);
    };
    if shadowed(ctx, &[name, sub]) {
        return Ok(None);
    }
    let Some((_, value)) = aliases.iter().find(|(n, _)| n == sub) else {
        return Ok(None);
    };
    let mut new_args = args.to_vec();
    if let Some(p) = new_args.iter().position(|a| a == sub) {
        new_args.remove(p);
    }
    Ok(Some(Command::Alias(super::alias::subcommand(
        &[name],
        sub,
        value,
        &new_args,
        ctx.globals,
        subcommands.contains(&sub.as_str()),
    )?)))
}

/// Whether the alias of the command path `path` is skipped.
fn shadowed(ctx: &Ctx<'_>, path: &[&str]) -> bool {
    ctx.shadow.iter().any(|s| s == path)
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

/// `ProviderHelpCommand.__call__`: a topic from the subcommand table
/// (`topics` and the topic names), or the provider page.
fn provider_help(args: &[String]) -> Result<Command, Fail> {
    if args.is_empty() || (args.len() == 1 && args[0] == "help") {
        return Ok(Command::Help(HelpRequest::Provider));
    }
    let topics = crate::help::topic_names();
    let mut parser = Parser::new();
    let mut sub = Action::positional("subcommand", Nargs::Optional);
    let mut choices = vec!["topics".to_string()];
    choices.extend(topics);
    sub.choices = Some(choices);
    parser.add(sub);
    let ns = match parser
        .parse_known_args(args)
        .map_err(|e| argparse_error(&e.to_string()))?
    {
        Parsed::Args { ns, .. } => ns,
        Parsed::Version => unreachable!("no version action"),
    };
    Ok(Command::Help(match &ns["subcommand"] {
        NsVal::Str(t) if t == "topics" => HelpRequest::TopicLister,
        NsVal::Str(t) => HelpRequest::Topic(t.clone()),
        _ => HelpRequest::Provider,
    }))
}

/// The help request of `help` for a customization's command (`args` is
/// exactly `['help']` after its subcommands).
fn custom_help(cli: &str, name: &str, args: &[String]) -> Option<Command> {
    crate::help::custom_request(&[cli.to_string(), name.to_string()], args).map(Command::Help)
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
    super::alias::extend_choices(&[cli], &mut choices)?;
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
        return Ok(Command::Help(HelpRequest::Service(cli.to_string())));
    }
    if !shadowed(ctx, &[cli, name])
        && let Some(value) = super::alias::find(&[cli], name)?
    {
        let proxied = table.iter().any(|(n, _)| n == name);
        return Ok(Command::Alias(super::alias::subcommand(
            &[cli],
            name,
            &value,
            &remaining,
            ctx.globals,
            proxied,
        )?));
    }
    let entry = table
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, e)| e.clone())
        .expect("choice");
    match entry {
        ServiceEntry::Operation(op) => {
            if super::commands::is_customized(cli, name) {
                if let Some(help) = custom_help(cli, name, &remaining) {
                    return Ok(help);
                }
                return Ok(Command::Custom {
                    path: vec![cli.to_string(), name.to_string()],
                    args: remaining.to_vec(),
                });
            }
            let op = model.operation(&op).expect("operation");
            let path = CommandPath { parent: cli, name };
            match operation::run_with(
                &op,
                path,
                &remaining,
                ctx,
                &mut service_hooks(cli, ctx, &remaining),
            )? {
                Invocation::Print(text) => Ok(Command::Print(text)),
                Invocation::Help { clear_required } => Ok(Command::Help(HelpRequest::Operation {
                    service: cli.to_string(),
                    name: name.to_string(),
                    clear_required,
                })),
                Invocation::Call(mut inv) => {
                    inv.cli_service = cli.to_string();
                    Ok(Command::Operation(*inv))
                }
            }
        }
        ServiceEntry::Wait => wait_command(cli, &model, &remaining, ctx),
        ServiceEntry::Custom => {
            if let Some(help) = custom_help(cli, name, &remaining) {
                return Ok(help);
            }
            Ok(Command::Custom {
                path: vec![cli.to_string(), name.to_string()],
                args: remaining.to_vec(),
            })
        }
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
    // A custom command that is a `ServiceOperation` of a removed model
    // operation (`ecs execute-command`) parses the model operation's
    // arguments.
    let op = match service_commands(cli, &model)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, e)| e)
    {
        Some(ServiceEntry::Operation(op)) => op,
        _ => match model
            .operation_names()
            .into_iter()
            .find(|o| super::arguments::xform_name(o, '-') == name)
        {
            Some(op) => op.to_string(),
            None => return Err(Fail::General(format!("unknown operation {cli} {name}"))),
        },
    };
    let op = model.operation(&op).expect("operation");
    let path = CommandPath { parent: cli, name };
    match operation::run_with(&op, path, args, ctx, hooks)? {
        Invocation::Print(text) => Ok(Command::Print(text)),
        Invocation::Help { clear_required } => {
            let req = HelpRequest::Operation {
                service: cli.to_string(),
                name: name.to_string(),
                clear_required,
            };
            crate::help::render_text(&req)
                .map(Command::Print)
                .map_err(Fail::General)
        }
        Invocation::Call(mut inv) => {
            inv.cli_service = cli.to_string();
            Ok(Command::Operation(*inv))
        }
    }
}

/// The handlers registered for every command of a service
/// (`before-building-argument-table-parser.emr.*`:
/// `emr.command.override_args_required_option`).
struct ServiceHooks {
    /// Keys of the profile's `emr` section.
    emr_config_keys: Vec<String>,
}

fn service_hooks(cli: &str, ctx: &Ctx<'_>, args: &[String]) -> ServiceHooks {
    let help = args.len() == 1 && args[0] == "help";
    ServiceHooks {
        emr_config_keys: if cli == "emr" && !help {
            ctx.config.nested_keys("emr")
        } else {
            Vec::new()
        },
    }
}

impl super::OperationHooks for ServiceHooks {
    fn table<'m>(&mut self, _op: &crate::model::Operation<'m>, table: &mut super::ArgTable<'m>) {
        override_emr_required(&self.emr_config_keys, table);
    }
}

/// `override_args_required_option`: arguments whose name is a key of the
/// profile's `emr` section are not required.
pub(crate) fn override_emr_required(keys: &[String], table: &mut super::ArgTable<'_>) {
    for (name, arg) in table.iter_mut() {
        if keys.contains(&name.replace('-', "_")) {
            arg.required = false;
        }
    }
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
        return Ok(Command::Help(HelpRequest::Wait(cli.to_string())));
    }
    let mut parser = Parser::new();
    let mut sub = Action::positional("subcommand", Nargs::Optional);
    // `WaitCommand._build_subcommand_table` adds the waiters after the
    // `building-command-table` handlers: an alias named like a waiter is
    // replaced by it.
    let mut choices: Vec<String> = Vec::new();
    super::alias::extend_choices(&[cli, "wait"], &mut choices)?;
    for (n, _) in &waiters {
        if !choices.contains(n) {
            choices.push(n.clone());
        }
    }
    sub.choices = Some(choices);
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
    if !waiters.iter().any(|(n, _)| n == sub)
        && let Some(value) = super::alias::find(&[cli, "wait"], sub)?
    {
        return Ok(Command::Alias(super::alias::subcommand(
            &[cli, "wait"],
            sub,
            &value,
            &new_args,
            ctx.globals,
            false,
        )?));
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
        Invocation::Help { clear_required } => Ok(Command::Help(HelpRequest::Waiter {
            service: cli.to_string(),
            name: sub.clone(),
            clear_required,
        })),
        Invocation::Call(inv) => Ok(Command::Wait(WaitInvocation {
            cli_service: cli.to_string(),
            service: model.name.clone(),
            waiter,
            params: inv.params,
        })),
    }
}
