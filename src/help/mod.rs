//! The help system (`awscli/help.py`): the help commands' documents
//! (`clidocs`), converted to roff by docutils' manpage writer
//! (`docutils`), formatted by groff / mandoc, and shown through the pager.

mod cache;
pub(crate) mod clidocs;
mod data;
mod docutils;
mod entities;
mod html;
pub mod pages;
mod render;
mod restdoc;

use serde_json::{Map, Value as Json};

use crate::bjson::{Arr, Node, Obj};

use crate::cli::arguments::xform_name;
use crate::cli::commands::{
    MainCommand, ServiceEntry, find_main, main_commands, service_commands, waiter_commands,
};
use crate::model::{Operation, ServiceModel, Shape};
use clidocs::{HelpArg, Kind, Page, TopicTagDb};
use data::HelpData;

/// A help page the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpRequest {
    /// `aws help`.
    Provider,
    /// `aws help topics`.
    TopicLister,
    /// `aws help <topic>`.
    Topic(String),
    /// `aws <service> help`.
    Service(String),
    /// `aws <service> <operation> help`; `clear_required` when
    /// `override_required_args` cleared the required flags.
    Operation {
        service: String,
        name: String,
        clear_required: bool,
    },
    /// `aws <service> wait help`.
    Wait(String),
    /// `aws <service> wait <waiter> help`.
    Waiter {
        service: String,
        name: String,
        clear_required: bool,
    },
    /// `help` of a `BasicCommand` at the command path.
    Custom(Vec<String>),
    /// `page` without the commands of its command table that aliases
    /// replaced (`BaseAliasCommand._UNDOCUMENTED`).
    Undocumented {
        page: Box<HelpRequest>,
        names: Vec<String>,
    },
    /// `page` of a command reached through the alias that replaced it in
    /// its command table (`InternalAliasSubCommand._proxied_sub_command`):
    /// its lineage, and so its `event_class`, starts at the command path's
    /// element `lineage`.
    Proxied {
        page: Box<HelpRequest>,
        lineage: usize,
    },
}

impl HelpRequest {
    /// The page without the alias wrappers.
    pub(crate) fn base(&self) -> &HelpRequest {
        match self {
            HelpRequest::Undocumented { page, .. } | HelpRequest::Proxied { page, .. } => {
                page.base()
            }
            req => req,
        }
    }
}

/// The arguments of `s3 sync` that the sync strategies add on
/// `building-arg-table.s3_sync` (`syncstrategy/register.py`), which a
/// `sync` with its own lineage does not emit.
const SYNC_STRATEGY_ARGS: &[&str] = &["size-only", "exact-timestamps", "delete"];

/// `WaitCommand.DESCRIPTION`.
const WAIT_DESCRIPTION: &str = "Wait until a particular condition is satisfied. Each subcommand polls an API until the listed requirement is met.";

/// The names of the topics (`TopicTagDB.get_all_topic_names`).
pub(crate) fn topic_names() -> Vec<String> {
    TopicTagDb::load().topic_names()
}

/// The `help` request for the custom command at `path` called with
/// `args`, if they ask for its help (`ArgTableArgParser.parse_known_args`
/// with `['help']` after the subcommands), or, with `flag` (`--help`), the
/// help of the command the subcommands in `args` lead to
/// (`BasicCommand.__call__` with `help_requested`). `None` when a
/// subcommand is not one of the command's subcommands.
pub fn custom_request(path: &[String], args: &[String], flag: bool) -> Option<HelpRequest> {
    let custom = HelpData::load("_custom")?;
    let mut path = path.to_vec();
    let mut rest = args.to_vec();
    loop {
        if rest.len() == 1 && rest[0] == "help" {
            break;
        }
        if flag {
            let prefix = format!("{} ", path.join(" "));
            if custom.command(prefix.trim_end()).is_none()
                || !custom
                    .command_paths()
                    .iter()
                    .any(|p| p.starts_with(&prefix))
            {
                break;
            }
            let Some(i) = rest.iter().position(|a| is_positional(a)) else {
                break;
            };
            custom.command(&format!("{prefix}{}", rest[i]))?;
            path.push(rest.remove(i));
            continue;
        }
        custom.command(&path.join(" "))?;
        let next = rest.first()?;
        custom.command(&format!("{} {next}", path.join(" ")))?;
        path.push(next.clone());
        rest.remove(0);
    }
    if custom.command(&path.join(" ")).is_some() {
        return Some(HelpRequest::Custom(path));
    }
    if path.len() == 2
        && let Some(data) = HelpData::load(&path[0])
        && let Some(table) = data.args("operations", &path[1])
    {
        return Some(HelpRequest::Operation {
            service: path[0].clone(),
            name: path[1].clone(),
            clear_required: flag && clears_required(table, &rest),
        });
    }
    None
}

/// An argument `argparse` matches as a positional in a parser without
/// options (`_parse_optional`: not starting with `-`, `-` alone, a
/// negative number, or containing a space).
fn is_positional(arg: &str) -> bool {
    let Some(tail) = arg.strip_prefix('-') else {
        return true;
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let negative = match tail.split_once('.') {
        Some((int, frac)) => digits(int) && !frac.is_empty() && digits(frac),
        None => !tail.is_empty() && digits(tail),
    };
    tail.is_empty() || negative || arg.contains(' ')
}

/// `OverrideRequiredArgsArgument.override_required_args` of the
/// arguments `table` for `args`.
fn clears_required(table: &Arr, args: &[String]) -> bool {
    let has = |name: &str| table.iter().any(|a| a["n"].as_str() == Some(name));
    ["cli-input-json", "cli-input-yaml"]
        .iter()
        .any(|n| has(n) && args.iter().any(|a| *a == format!("--{n}")))
        || (has("generate-cli-skeleton")
            && args
                .iter()
                .position(|a| a == "--generate-cli-skeleton")
                .is_some_and(|i| args.get(i + 1).map(String::as_str) != Some("output")))
}

/// Owned shapes that the argument models of an argument table borrow.
#[derive(Default)]
struct Owned {
    models: Vec<ServiceModel>,
    members: Vec<&'static Obj>,
    /// Shape definitions with members a customization removed:
    /// (name, definition).
    defs: Vec<(String, &'static Obj)>,
}

struct ModelRef {
    source: Source,
    top: String,
    member: Option<usize>,
    /// The kept member names of the top shape.
    members: Option<Vec<String>>,
}

enum Source {
    Input,
    Svc,
    Own(usize),
}

fn leak_obj(map: Map<String, Json>) -> &'static Obj {
    Node::leak(&Json::Object(map)).as_object().expect("object")
}

/// The owned parts of the argument models of `records`.
fn collect_models(records: &Arr, data: &HelpData, owned: &mut Owned) -> Vec<Option<ModelRef>> {
    records
        .iter()
        .map(|r| {
            if let Some(m) = r.get("m").and_then(Node::as_str) {
                return Some(ModelRef {
                    source: Source::Input,
                    top: m.to_string(),
                    member: None,
                    members: None,
                });
            }
            let s = data.model(r.get("s")?)?;
            let top = s.get("top")?.as_str()?.to_string();
            let member = s.get("member").and_then(Node::as_object).map(|m| {
                owned.members.push(m);
                owned.members.len() - 1
            });
            let members = s.get("members").and_then(Node::as_array).map(|a| {
                a.iter()
                    .filter_map(Node::as_str)
                    .map(str::to_string)
                    .collect()
            });
            let source = if s.get("svc").and_then(Node::as_bool) == Some(true) {
                Source::Svc
            } else {
                owned.models.push(ServiceModel::from_shapes(
                    "custom",
                    s.get("shapes")?.to_json(),
                ));
                Source::Own(owned.models.len() - 1)
            };
            Some(ModelRef {
                source,
                top,
                member,
                members,
            })
        })
        .collect()
}

/// Copies of the shape definitions whose members a customization removed.
fn collect_defs(
    refs: &[Option<ModelRef>],
    owned: &mut Owned,
    svc: Option<&ServiceModel>,
) -> Vec<Option<usize>> {
    refs.iter()
        .map(|r| {
            let r = r.as_ref()?;
            let keep = r.members.as_ref()?;
            let model = match r.source {
                Source::Input => return None,
                Source::Svc => svc?,
                Source::Own(i) => &owned.models[i],
            };
            let mut def = model.shape_def(&r.top)?.to_map();
            if let Some(Json::Object(members)) = def.get_mut("members") {
                members.retain(|k, _| keep.contains(k));
            }
            owned.defs.push((r.top.clone(), leak_obj(def)));
            Some(owned.defs.len() - 1)
        })
        .collect()
}

fn resolve<'a>(
    r: &ModelRef,
    def: Option<usize>,
    owned: &'a Owned,
    svc: Option<&'a ServiceModel>,
    input: Option<Shape<'a>>,
) -> Option<Shape<'a>> {
    let mut s = match r.source {
        Source::Input => return input?.member_shape(&r.top),
        Source::Svc => svc?.shape(&r.top)?,
        Source::Own(i) => owned.models[i].shape(&r.top)?,
    };
    s.member = r.member.map(|i| owned.members[i]);
    if let Some(d) = def {
        s.name = &owned.defs[d].0;
        s.def = owned.defs[d].1;
    }
    Some(s)
}

#[allow(clippy::too_many_arguments)]
fn help_args<'a>(
    records: &Arr,
    refs: &[Option<ModelRef>],
    defs: &[Option<usize>],
    data: &HelpData,
    owned: &'a Owned,
    svc: Option<&'a ServiceModel>,
    input: Option<Shape<'a>>,
    clear_required: bool,
) -> Vec<HelpArg<'a>> {
    records
        .iter()
        .zip(refs.iter().zip(defs))
        .map(|(r, (mref, def))| {
            let name = r["n"].as_str().unwrap_or_default().to_string();
            let model = mref
                .as_ref()
                .and_then(|m| resolve(m, *def, owned, svc, input));
            let flag = |k: &str| r.get(k).and_then(Node::as_bool).unwrap_or(false);
            let string = |k: &str| r.get(k).and_then(Node::as_str).map(str::to_string);
            HelpArg {
                cli_name: string("c").unwrap_or_else(|| format!("--{name}")),
                documentation: match r.get("d") {
                    Some(d) => data.doc(d).to_string(),
                    None => model
                        .as_ref()
                        .and_then(|m| m.get_str("documentation"))
                        .unwrap_or_default()
                        .to_string(),
                },
                cli_type_name: string("t").unwrap_or_else(|| {
                    model
                        .as_ref()
                        .map(|m| m.type_name().to_string())
                        .unwrap_or_default()
                }),
                required: flag("r") && !clear_required,
                document_as_required: flag("ar"),
                group_name: string("g"),
                positional_arg: flag("p"),
                nargs: r
                    .get("na")
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)),
                synopsis: string("sy"),
                undocumented: flag("u"),
                model,
                name,
            }
        })
        .collect()
}

/// The botocore operation behind the command `name` of `cli`.
fn find_operation<'m>(cli: &str, model: &'m ServiceModel, name: &str) -> Option<Operation<'m>> {
    let op = match service_commands(cli, model)
        .into_iter()
        .find(|(n, _)| n == name)
    {
        Some((_, ServiceEntry::Operation(op))) => op,
        _ => model
            .operation_names()
            .into_iter()
            .find(|o| xform_name(o, '-') == name)?
            .to_string(),
    };
    model.operation(&op)
}

fn service_name(cli: &str) -> Result<&'static str, String> {
    match find_main(cli) {
        Some(MainCommand::Service(_, service)) => Ok(service),
        _ => Err(format!("unknown service command {cli}")),
    }
}

/// `WaiterStateDocBuilder.build_waiter_state_description`.
fn waiter_description(config: &Map<String, Json>) -> String {
    let py = |v: &Json| match v {
        Json::String(s) => s.clone(),
        Json::Bool(true) => "True".to_string(),
        Json::Bool(false) => "False".to_string(),
        Json::Null => "None".to_string(),
        other => other.to_string(),
    };
    let mut description = config
        .get("description")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    if description.is_empty() {
        description = "Wait until ".to_string();
        let acceptors = config
            .get("acceptors")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some(a) = acceptors.iter().find(|a| a["state"] == "success") {
            let matcher = a["matcher"].as_str().unwrap_or_default();
            let template = match matcher {
                "error" => "%s is thrown ",
                "path" => "%s ",
                "pathAll" => "%s for all elements ",
                "pathAny" => "%s for any element ",
                _ => "%s response is received ",
            };
            let mut template = template.to_string();
            if matches!(matcher, "path" | "pathAny" | "pathAll") {
                template = format!("JMESPath query {} returns {template}", py(&a["argument"]));
            }
            description.push_str(&template.replacen("%s", &py(&a["expected"]), 1));
        }
        let op = config
            .get("operation")
            .and_then(Json::as_str)
            .unwrap_or_default();
        description.push_str(&format!(
            "when polling with ``{}``.",
            xform_name(op, '_').replace('_', "-")
        ));
    }
    description.push_str(&format!(
        " It will poll every {} seconds until a successful state has been reached. This will exit with a return code of 255 after {} failed checks.",
        py(&config["delay"]),
        py(&config["maxAttempts"])
    ));
    description
}

/// The reStructuredText of the help page `req` (`doc.getvalue()` after
/// `generate_events`).
pub fn document(req: &HelpRequest) -> Result<String, String> {
    document_with(req, &|_| {})
}

/// [`document`] with `adjust` applied to the page before it is generated.
fn document_with(req: &HelpRequest, adjust: &dyn Fn(&mut Page<'_>)) -> Result<String, String> {
    let rst = match req {
        HelpRequest::Undocumented { page, names } => {
            return document_with(page, &|p: &mut Page<'_>| {
                adjust(p);
                p.commands.retain(|c| !names.contains(c));
            });
        }
        HelpRequest::Proxied { page, lineage } => {
            let sync = **page == HelpRequest::Custom(vec!["s3".to_string(), "sync".to_string()]);
            return document_with(page, &|p: &mut Page<'_>| {
                adjust(p);
                if sync {
                    p.args
                        .retain(|a| !SYNC_STRATEGY_ARGS.contains(&a.name.as_str()));
                }
                p.event_class = p
                    .event_class
                    .split('.')
                    .skip(*lineage)
                    .collect::<Vec<_>>()
                    .join(".");
            });
        }
        HelpRequest::Provider => {
            let cli = crate::model::cli_data();
            let s = |k: &str| {
                cli.get(k)
                    .and_then(Node::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let custom = HelpData::load("_custom").ok_or("missing help data")?;
            let undocumented = custom.undocumented();
            let mut page = Page::new(
                "awsr",
                "aws",
                Kind::Provider {
                    description: s("description"),
                    synopsis: s("synopsis"),
                    usage: s("help_usage"),
                },
                Vec::new(),
            );
            let mut commands: Vec<String> = main_commands()
                .map(|c| match c {
                    MainCommand::Service(n, _) | MainCommand::Custom(n) => n.to_string(),
                })
                .collect();
            commands.push("help".to_string());
            page.commands = commands
                .into_iter()
                .filter(|c| !undocumented.contains(&c.as_str()))
                .collect();
            page.has_commands = true;
            page.related_items = vec!["awsr help topics".to_string()];
            adjust(&mut page);
            page.generate()
        }
        HelpRequest::TopicLister => {
            let mut page = Page::new("topics", "topics", Kind::TopicLister, Vec::new());
            adjust(&mut page);
            page.generate()
        }
        HelpRequest::Topic(name) => {
            let mut page = Page::new(name, &format!("topics.{name}"), Kind::Topic, Vec::new());
            adjust(&mut page);
            page.generate()
        }
        HelpRequest::Service(cli) => {
            let model = ServiceModel::load(service_name(cli)?).map_err(|e| e.to_string())?;
            let data = HelpData::load(cli).ok_or("missing help data")?;
            let undocumented = data.undocumented();
            let mut page = Page::new(
                cli,
                cli,
                Kind::Service {
                    documentation: model.documentation().to_string(),
                },
                Vec::new(),
            );
            let mut commands: Vec<String> = service_commands(cli, &model)
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            commands.push("help".to_string());
            page.commands = commands
                .into_iter()
                .filter(|c| !undocumented.contains(&c.as_str()))
                .collect();
            page.has_commands = true;
            adjust(&mut page);
            page.generate()
        }
        HelpRequest::Operation {
            service,
            name,
            clear_required,
        } => {
            let model = ServiceModel::load(service_name(service)?).map_err(|e| e.to_string())?;
            let data = HelpData::load(service).ok_or("missing help data")?;
            let op = find_operation(service, &model, name).ok_or("unknown operation")?;
            let records = data.args("operations", name).ok_or("missing help data")?;
            let mut owned = Owned::default();
            let refs = collect_models(records, &data, &mut owned);
            let defs = collect_defs(&refs, &mut owned, Some(&model));
            let args = help_args(
                records,
                &refs,
                &defs,
                &data,
                &owned,
                Some(&model),
                op.input(),
                *clear_required,
            );
            let documentation = op
                .get("documentation")
                .and_then(Node::as_str)
                .unwrap_or_default()
                .to_string();
            let no_output = data.json["no_output"]
                .as_array()
                .is_some_and(|a| a.iter().any(|n| n.as_str() == Some(name)));
            Page::new(
                name,
                &format!("{service}.{name}"),
                Kind::Operation {
                    op,
                    documentation,
                    output: if no_output { None } else { op.output() },
                },
                args,
            )
            .adjusted(adjust)
            .generate()
        }
        HelpRequest::Wait(cli) => {
            let model = ServiceModel::load(service_name(cli)?).map_err(|e| e.to_string())?;
            let mut page = Page::new(
                "wait",
                &format!("{cli}.wait"),
                Kind::Basic {
                    description: WAIT_DESCRIPTION.to_string(),
                    synopsis: String::new(),
                    examples: String::new(),
                    waiter: true,
                },
                Vec::new(),
            );
            let waiters = waiter_commands(&model);
            page.has_commands = !waiters.is_empty();
            let deprecated = |waiter: &str| {
                model
                    .waiters()
                    .and_then(|w| w.get(waiter))
                    .and_then(|c| c.get("operation"))
                    .and_then(Node::as_str)
                    .and_then(|op| model.operation(op))
                    .is_some_and(|op| op.is_deprecated())
            };
            page.commands = waiters
                .into_iter()
                .filter(|(_, w)| !deprecated(w))
                .map(|(n, _)| n)
                .collect();
            adjust(&mut page);
            page.generate()
        }
        HelpRequest::Waiter {
            service,
            name,
            clear_required,
        } => {
            let model = ServiceModel::load(service_name(service)?).map_err(|e| e.to_string())?;
            let data = HelpData::load(service).ok_or("missing help data")?;
            let waiter = waiter_commands(&model)
                .into_iter()
                .find(|(n, _)| n == name)
                .map(|(_, w)| w)
                .ok_or("unknown waiter")?;
            let config = model
                .waiters()
                .and_then(|w| w.get(&waiter))
                .and_then(Node::as_object)
                .ok_or("unknown waiter")?;
            let op_name = config
                .get("operation")
                .and_then(Node::as_str)
                .ok_or("unknown waiter")?;
            let op = model.operation(op_name).ok_or("unknown operation")?;
            let records = data.args("waiters", name).ok_or("missing help data")?;
            let mut owned = Owned::default();
            let refs = collect_models(records, &data, &mut owned);
            let defs = collect_defs(&refs, &mut owned, Some(&model));
            let args = help_args(
                records,
                &refs,
                &defs,
                &data,
                &owned,
                Some(&model),
                op.input(),
                *clear_required,
            );
            Page::new(
                name,
                &format!("{service}.wait.{name}"),
                Kind::Operation {
                    op,
                    documentation: waiter_description(&config.to_map()),
                    output: None,
                },
                args,
            )
            .adjusted(adjust)
            .generate()
        }
        HelpRequest::Custom(path) => {
            let custom = HelpData::load("_custom").ok_or("missing help data")?;
            let spec = custom.command(&path.join(" ")).ok_or("unknown command")?;
            let s = |k: &str| {
                spec.get(k)
                    .and_then(Node::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let records = spec
                .get("args")
                .and_then(Node::as_array)
                .unwrap_or(Node::empty_array());
            let mut owned = Owned::default();
            let refs = collect_models(records, &custom, &mut owned);
            let defs = collect_defs(&refs, &mut owned, None);
            let args = help_args(records, &refs, &defs, &custom, &owned, None, None, false);
            let mut page = Page::new(
                &s("name"),
                &path.join("."),
                Kind::Basic {
                    description: s("description"),
                    synopsis: s("synopsis"),
                    examples: s("examples"),
                    waiter: false,
                },
                args,
            );
            let names = |k: &str| -> Vec<String> {
                spec.get(k)
                    .and_then(Node::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Node::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let undocumented = names("undocumented");
            let commands = names("commands");
            page.has_commands = !commands.is_empty();
            page.commands = commands
                .into_iter()
                .filter(|c| !undocumented.contains(c))
                .collect();
            adjust(&mut page);
            page.generate()
        }
    };
    rst.map(|rst| crate::progname::help(&rst))
}

/// Whether the page's command table has entries (`BasicHelp.url`).
fn has_commands(req: &HelpRequest) -> bool {
    match req.base() {
        HelpRequest::Custom(path) => HelpData::load("_custom")
            .and_then(|c| {
                c.command(&path.join(" "))
                    .and_then(|s| s.get("commands"))
                    .and_then(Node::as_array)
                    .map(|a| !a.is_empty())
            })
            .unwrap_or(false),
        _ => false,
    }
}

/// The groff output of the page (`PosixHelpRenderer._convert_doc_content`),
/// with docutils' messages written to stderr.
fn convert(req: &HelpRequest, device: &str) -> Result<Vec<u8>, String> {
    if device == "ascii"
        && let Some((stderr, output)) = pages::lookup(req)
    {
        std::io::Write::write_all(&mut std::io::stderr(), &stderr).map_err(|e| e.to_string())?;
        return Ok(output);
    }
    let cached = render::formatter().and_then(|f| cache::path(req, device, f));
    if let Some((stderr, output)) = cached.as_deref().and_then(cache::read) {
        std::io::Write::write_all(&mut std::io::stderr(), &stderr).map_err(|e| e.to_string())?;
        return Ok(output);
    }
    let rst = document(req)?;
    let published = docutils::publish_man(&rst)?;
    if !published.stderr.is_empty() {
        eprint!("{}", published.stderr);
    }
    let output = render::groff(published.man.as_bytes(), device)?;
    if let Some(path) = cached {
        cache::write(&path, published.stderr.as_bytes(), &output);
    }
    Ok(output)
}

/// The formatted page as text, for callers that print rather than page.
pub(crate) fn render_text(req: &HelpRequest) -> Result<String, String> {
    convert(req, "ascii").map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// `HelpCommand.__call__`: render the page with the `cli_help_output`
/// renderer. The provider page of the `help` command is the one of the
/// command table, created before `--profile` applies; `--help` creates it
/// after (`CLIDriver._get_command`).
pub(crate) fn run(
    ctx: &crate::driver::Context,
    req: &HelpRequest,
) -> Result<crate::error::ExitCode, crate::error::CliError> {
    use crate::config::ConfigLookup;
    use crate::error::{CliError, ExitCode};
    let fail = |e: String| {
        CliError::new(
            ExitCode::General,
            crate::output::errors::render_general_error(&e),
        )
    };
    let format = if *req.base() == HelpRequest::Provider
        && !ctx.globals.help
        && ctx.globals.profile.is_some()
    {
        crate::config::Config::new(&crate::cli::GlobalArgs {
            profile: None,
            ..ctx.globals.clone()
        })
        .ok()
        .and_then(|c| c.config_variable("cli_help_output"))
    } else {
        ctx.session.config_variable("cli_help_output")
    };
    match format.as_deref() {
        Some("browser") => {
            render::open_in_browser(&convert(req, "html").map_err(fail)?).map_err(fail)?
        }
        Some("url") => {
            render::page(render::url(req, has_commands(req)).as_bytes()).map_err(fail)?
        }
        _ => render::page(&convert(req, "ascii").map_err(fail)?).map_err(fail)?,
    }
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference's documents (`doc.getvalue()`) for a page of each kind.
    #[test]
    fn documents_match_the_reference() {
        let cases = [
            (
                HelpRequest::TopicLister,
                include_str!("testdata/help-topics.rst"),
            ),
            (
                HelpRequest::Wait("proton".to_string()),
                include_str!("testdata/proton-wait.rst"),
            ),
            (
                HelpRequest::Operation {
                    service: "sts".to_string(),
                    name: "get-caller-identity".to_string(),
                    clear_required: false,
                },
                include_str!("testdata/sts-get-caller-identity.rst"),
            ),
            (
                HelpRequest::Custom(vec!["configure".to_string(), "list".to_string()]),
                include_str!("testdata/configure-list.rst"),
            ),
        ];
        for (req, expected) in cases {
            assert_eq!(
                document(&req).unwrap(),
                crate::progname::help(expected),
                "{req:?}"
            );
        }
    }

    #[test]
    fn custom_requests() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            custom_request(&s(&["s3"]), &s(&["cp", "help"]), false),
            Some(HelpRequest::Custom(s(&["s3", "cp"])))
        );
        assert_eq!(
            custom_request(&s(&["ecs", "execute-command"]), &s(&["help"]), false),
            Some(HelpRequest::Operation {
                service: "ecs".to_string(),
                name: "execute-command".to_string(),
                clear_required: false,
            })
        );
        assert_eq!(
            custom_request(&s(&["s3"]), &s(&["cp", "a", "b"]), false),
            None
        );
        assert_eq!(
            custom_request(&s(&["s3"]), &s(&["--debug", "cp", "a"]), true),
            Some(HelpRequest::Custom(s(&["s3", "cp"])))
        );
        assert_eq!(
            custom_request(&s(&["s3"]), &[], true),
            Some(HelpRequest::Custom(s(&["s3"])))
        );
        assert_eq!(custom_request(&s(&["s3"]), &s(&["nosuch"]), true), None);
        assert_eq!(
            custom_request(&s(&["cli-dev"]), &s(&["wizard-dev"]), true),
            Some(HelpRequest::Custom(s(&["cli-dev", "wizard-dev"])))
        );
    }
}
