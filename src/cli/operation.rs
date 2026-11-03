//! One model-driven operation command: `ServiceOperation.__call__`
//! (`awscli/clidriver.py`) with the generic `before-building-argument-table-parser`,
//! `operation-args-parsed`, and `calling-command` customizations
//! (`cliinput.py`, `generatecliskeleton.py`, `paginate.py`,
//! `toplevelbool.py`, `ec2/runinstances.py`, `ec2/paginate.py`,
//! `cloudsearchdomain.py`), and the client-side steps that precede
//! botocore's validation (`binaryformat.py` `base64_decode_input_blobs`,
//! the `before-parameter-build` handlers of `ec2/addcount.py`,
//! `ec2/runinstances.py`, `ec2/protocolarg.py`).

use indexmap::IndexMap;
use num_bigint::BigInt;

use super::argparse::{NsVal, Parsed};
use super::argprocess::{ArgContext, load_cli_arg, param_error, unpack_argument};
use super::arguments::{
    Arg, ArgKind, ArgTable, CommandPath, build_argument_table_with, operation_parser,
    paginator_input_tokens, xform_name,
};
use super::difflib::get_close_matches;
use super::globals::cli_options;
use super::paramfile;
use super::render::argparse_error;
use super::shorthand::int_of;
use super::{Ctx, Fail, OperationInvocation, skeleton, validate};
use crate::model::{Operation, Shape};
use crate::paginate::PaginationConfig;
use crate::value::{Map, Value};

/// The handlers a service customization registers for one operation
/// (`building-argument-table.<service>.<operation>`,
/// `operation-args-parsed`, the customization's `add_to_params`, and the
/// `calling-command` / `before-parameter-build` handlers).
pub(crate) trait OperationHooks {
    /// Change the argument table (add `CustomArgument`s, remove or rename
    /// model arguments).
    fn table<'m>(&mut self, _op: &Operation<'m>, _table: &mut ArgTable<'m>) {}
    /// `operation-args-parsed.<service>.<operation>` (before the generic
    /// handlers): the parsed namespace and `parsed_globals.paginate`.
    fn args_parsed(
        &mut self,
        _ns: &IndexMap<String, NsVal>,
        _paginate: &mut bool,
    ) -> Result<(), Fail> {
        Ok(())
    }
    /// `add_to_params` of an [`ArgKind::Custom`] argument, with the value
    /// after `load-cli-arg` (`None` when not given).
    fn add_to_params(
        &mut self,
        _name: &str,
        _value: Option<Value>,
        _params: &mut Map,
    ) -> Result<(), Fail> {
        Ok(())
    }
    /// The `calling-command` / `before-parameter-build` handlers: the
    /// parameters before the client-side handlers and validation.
    fn before_call(&mut self, _params: &mut Map) -> Result<(), Fail> {
        Ok(())
    }
}

struct NoHooks;

impl OperationHooks for NoHooks {}

pub(crate) enum Invocation {
    /// Text for stdout (`--generate-cli-skeleton`).
    Print(String),
    /// The `help` positional: `OperationHelpCommand`, with whether
    /// `override_required_args` cleared the required flags.
    Help {
        clear_required: bool,
    },
    Call(Box<OperationInvocation>),
}

/// `get_option_suggestion_candidates`.
pub(crate) fn option_suggestion_candidates(table: Option<&ArgTable<'_>>) -> Vec<String> {
    let mut out: Vec<String> = table
        .map(|t| {
            t.values()
                .filter(|a| a.cli_name().starts_with("--") && !a.undocumented)
                .map(|a| a.cli_name()[2..].to_string())
                .collect()
        })
        .unwrap_or_default();
    out.extend(cli_options().into_iter().map(|(n, _)| n));
    out
}

/// `format_unknown_options_error`.
pub(crate) fn format_unknown_options_error(unknown: &[String], candidates: &[String]) -> String {
    let mut msg = format!("Unknown options: {}", unknown.join(", "));
    let mut suggestions: Vec<String> = Vec::new();
    for arg in unknown {
        let Some(name) = arg.strip_prefix("--") else {
            continue;
        };
        let name = name.split('=').next().unwrap_or_default();
        for m in get_close_matches(name, candidates, 3, 0.8) {
            let s = format!("  * --{m} (instead of --{name})");
            if !suggestions.contains(&s) {
                suggestions.push(s);
            }
        }
    }
    if !suggestions.is_empty() {
        msg.push_str("\n\nMaybe you meant:\n");
        msg.push_str(&suggestions.join("\n"));
    }
    msg
}

fn ns_value(v: &NsVal) -> Value {
    match v {
        NsVal::None | NsVal::NotSpecified => Value::Null,
        NsVal::Bool(b) => Value::Bool(*b),
        NsVal::Str(s) => Value::Str(s.clone()),
        NsVal::Int(i) => Value::Int(i.clone()),
        NsVal::Float(f) => Value::Float(*f),
        NsVal::List(l) => Value::List(l.iter().map(ns_value).collect()),
    }
}

/// `OverrideRequiredArgsArgument.override_required_args` for
/// `--cli-input-json`, `--cli-input-yaml`, and `--generate-cli-skeleton`.
fn override_required_args(table: &mut ArgTable<'_>, args: &[String]) -> bool {
    let mut clear = false;
    for name in ["cli-input-json", "cli-input-yaml"] {
        if table.contains_key(name) && args.iter().any(|a| *a == format!("--{name}")) {
            clear = true;
        }
    }
    if table.contains_key("generate-cli-skeleton")
        && let Some(i) = args.iter().position(|a| a == "--generate-cli-skeleton")
        && args.get(i + 1).map(String::as_str) != Some("output")
    {
        clear = true;
    }
    if clear {
        for a in table.values_mut() {
            a.required = false;
        }
    }
    clear
}

struct State<'m> {
    params: Map,
    outfile: Option<String>,
    paginate: bool,
    _marker: std::marker::PhantomData<&'m ()>,
}

/// Run `ServiceOperation.__call__` up to the client call: parse, the
/// `operation-args-parsed` handlers, `_build_call_parameters`, the
/// `calling-command` handlers, and `CLIOperationCaller.invoke` /
/// `WaiterCaller.invoke` (client-side handlers and validation).
pub(crate) fn run(
    op: &Operation<'_>,
    path: CommandPath<'_>,
    args: &[String],
    ctx: &Ctx<'_>,
) -> Result<Invocation, Fail> {
    run_with(op, path, args, ctx, &mut NoHooks)
}

/// [`run`] with a service customization's handlers.
pub(crate) fn run_with<'m>(
    op: &Operation<'m>,
    path: CommandPath<'_>,
    args: &[String],
    ctx: &Ctx<'_>,
    hooks: &mut dyn OperationHooks,
) -> Result<Invocation, Fail> {
    let waiter = path.parent == "wait";
    let (mut table, mut extras) =
        build_argument_table_with(op, path, &mut |table| hooks.table(op, table));
    let clear_required = override_required_args(&mut table, args);

    if args.len() == 1 && args[0] == "help" {
        return Ok(Invocation::Help { clear_required });
    }
    let parser = operation_parser(&table, true);
    let (ns, mut remaining) = match parser
        .parse_known_args(args)
        .map_err(|e| argparse_error(&e.to_string()))?
    {
        Parsed::Args { ns, extras } => (ns, extras),
        Parsed::Version => unreachable!("no version action"),
    };
    match ns.get("help") {
        Some(NsVal::Str(h)) if h == "help" => {
            return Ok(Invocation::Help { clear_required });
        }
        Some(NsVal::Str(h)) if !h.is_empty() => remaining.push(h.clone()),
        _ => {}
    }
    if !remaining.is_empty() {
        return Err(Fail::Unknown(format_unknown_options_error(
            &remaining,
            &option_suggestion_candidates(Some(&table)),
        )));
    }

    let mut state = State {
        params: Map::new(),
        outfile: None,
        paginate: !ctx.globals.no_paginate,
        _marker: std::marker::PhantomData,
    };

    hooks.args_parsed(&ns, &mut state.paginate)?;
    for (positive, negative) in &extras.bool_pairs {
        let p = positive.trim_start_matches('-').replace('-', "_");
        let n = negative.trim_start_matches('-').replace('-', "_");
        if ns.get(&p) != Some(&NsVal::NotSpecified) && ns.get(&n) != Some(&NsVal::NotSpecified) {
            return Err(Fail::ParamValidation(format!(
                "Cannot specify both the \"{positive}\" option and the \"{negative}\" option."
            )));
        }
    }
    if path.parent == "ec2"
        && path.name == "run-instances"
        && ns.get("network_interfaces").is_some_and(NsVal::is_truthy)
    {
        for key in [
            "secondary_private_ip_addresses",
            "secondary_private_ip_address_count",
            "associate_public_ip_address",
        ] {
            if ns.get(key).is_some_and(NsVal::is_truthy) {
                return Err(Fail::ParamValidation(
                    "Mixing the --network-interfaces option with the simple, scalar options is not supported."
                        .to_string(),
                ));
            }
        }
    }
    if let Some(config) = extras.paginator {
        for token in paginator_input_tokens(config) {
            let py = xform_name(&token, '-').replace('-', "_");
            match ns.get(&py) {
                None => {
                    return Err(Fail::General(format!(
                        "'Namespace' object has no attribute '{py}'"
                    )));
                }
                Some(v) if !v.is_none() && py != "start_token" && py != "max_items" => {
                    state.paginate = false;
                }
                _ => {}
            }
        }
        if !state.paginate {
            let shadowed: Vec<String> = extras
                .shadowed
                .keys()
                .map(|k| k.replace('-', "_"))
                .collect();
            let used: Vec<&str> = ["starting_token", "page_size", "max_items"]
                .into_iter()
                .filter(|p| {
                    !shadowed.iter().any(|s| s == p) && ns.get(*p).is_some_and(NsVal::is_truthy)
                })
                .collect();
            if !used.is_empty() {
                let names: Vec<String> = used
                    .iter()
                    .map(|p| format!("--{}", p.replace('_', "-")))
                    .collect();
                return Err(Fail::ParamValidation(format!(
                    "Cannot specify --no-paginate along with pagination arguments: {}",
                    names.join(", ")
                )));
            }
            for (k, v) in std::mem::take(&mut extras.shadowed) {
                table.insert(k, v);
            }
        }
    }

    let service_name = op.model.name.as_str();
    let op_cli = xform_name(op.name, '-');
    for arg in table.values() {
        let py = arg.py_name();
        let Some(raw) = ns.get(&py) else { continue };
        let raw_value = match raw {
            NsVal::NotSpecified => None,
            v => Some(ns_value(v)),
        };
        let value = match &raw_value {
            Some(v) => match load_cli_arg(&arg.cli_name(), v)? {
                Some(o) => Some(o),
                None => Some(v.clone()),
            },
            None => None,
        };
        if let ArgKind::Custom(_) = arg.kind {
            let value = value.filter(|v| !matches!(v, Value::Null));
            hooks.add_to_params(&arg.name, value, &mut state.params)?;
            continue;
        }
        add_to_params(arg, raw, value, &mut state, service_name, &op_cli)?;
    }

    let mut printed: Option<String> = None;
    if table.contains_key("cli-input-json") {
        for yaml in [false, true] {
            add_cli_input(&ns, yaml, &mut state.params)?;
        }
    }
    if table.contains_key("generate-cli-skeleton")
        && let Some(NsVal::Str(kind)) = ns.get("generate_cli_skeleton")
        && !kind.is_empty()
    {
        match kind.as_str() {
            "input" => printed = Some(skeleton::input_json(op.input().as_ref())),
            "yaml-input" => printed = Some(super::yamlskel::input_yaml(op.input().as_ref())),
            _ => {
                let fake = match op.output() {
                    Some(s) => skeleton::generate(&s, true),
                    None => Value::map(),
                };
                if let Some(output) = op.output()
                    && let Some(report) = validate::validate(&fake, &output)
                {
                    return Err(Fail::ParamValidation(report));
                }
                let mut params = std::mem::take(&mut state.params);
                hooks.before_call(&mut params)?;
                let mut params = Value::Map(params);
                client_params(op, &mut params, ctx)?;
                return Ok(Invocation::Call(Box::new(OperationInvocation {
                    cli_service: String::new(),
                    service: op.model.name.clone(),
                    operation: op.name.to_string(),
                    params,
                    paginate: false,
                    pagination: PaginationConfig::default(),
                    outfile: state.outfile,
                    stub_response: Some(fake),
                })));
            }
        }
    }
    if let Some(config) = extras.paginator {
        let tokens = paginator_input_tokens(config);
        if state.params.keys().any(|k| tokens.contains(k)) {
            state.paginate = false;
        }
    }
    if path.parent == "ec2"
        && matches!(path.name, "describe-volumes" | "describe-snapshots")
        && state.paginate
    {
        inject_page_size(path.name, &mut state.params);
    }
    if let Some(text) = printed {
        return Ok(Invocation::Print(text));
    }
    if path.parent == "cloudsearchdomain" && ctx.globals.endpoint_url.is_none() {
        return Err(Fail::ParamValidation(
            "--endpoint-url is required for cloudsearchdomain commands".to_string(),
        ));
    }

    let paginate = !waiter && extras.paginator.is_some() && state.paginate;
    let mut params = state.params;
    let mut pagination = PaginationConfig::default();
    if paginate && let Some(Value::Map(pc)) = params.shift_remove("PaginationConfig") {
        pagination = pagination_config(&pc)?;
    }
    hooks.before_call(&mut params)?;
    let mut params = Value::Map(params);
    client_params(op, &mut params, ctx)?;
    Ok(Invocation::Call(Box::new(OperationInvocation {
        cli_service: String::new(),
        service: op.model.name.clone(),
        operation: op.name.to_string(),
        params,
        paginate,
        pagination,
        outfile: state.outfile,
        stub_response: None,
    })))
}

/// The `add_to_params` of each argument class. `ec2/addcount.py` raises
/// `ParamValidationError` without importing it, so a malformed `--count`
/// ends in the `NameError` of its bare `except`.
fn add_to_params(
    arg: &Arg<'_>,
    raw: &NsVal,
    value: Option<Value>,
    state: &mut State<'_>,
    service: &str,
    operation: &str,
) -> Result<(), Fail> {
    let params = &mut state.params;
    let cli_name = arg.cli_name();
    let unpack = |shape: &Shape<'_>, v: &Value| {
        unpack_argument(
            &ArgContext {
                name: &arg.name,
                cli_name: &cli_name,
                shape,
                service,
                operation,
            },
            v,
        )
    };
    match &arg.kind {
        ArgKind::Cli | ArgKind::List => {
            let Some(v) = value.filter(|v| !matches!(v, Value::Null)) else {
                return Ok(());
            };
            let shape = arg.shape.as_ref().expect("model argument");
            let unpacked = unpack(shape, &v)?;
            params.insert(arg.serialized_name.clone(), unpacked);
        }
        ArgKind::Boolean { .. } => {
            if let Some(v) = value.filter(|v| !matches!(v, Value::Null)) {
                params.insert(arg.serialized_name.clone(), v);
            }
        }
        ArgKind::PositiveBool => match raw {
            NsVal::NotSpecified => {}
            NsVal::None => {
                let mut m = Map::new();
                m.insert("Value".to_string(), Value::Bool(true));
                params.insert(arg.serialized_name.clone(), Value::Map(m));
            }
            _ => {
                let v = value.unwrap_or(Value::Null);
                let shape = arg.shape.as_ref().expect("model argument");
                let unpacked = unpack(shape, &v)?;
                params.insert(arg.serialized_name.clone(), unpacked);
            }
        },
        ArgKind::NegativeBool => {
            if !matches!(raw, NsVal::NotSpecified) && value.as_ref().is_some_and(Value::is_truthy) {
                let mut m = Map::new();
                m.insert("Value".to_string(), Value::Bool(false));
                params.insert(arg.serialized_name.clone(), Value::Map(m));
            }
        }
        ArgKind::Page { serialized, .. } => {
            let Some(v) = value.filter(|v| !matches!(v, Value::Null)) else {
                return Ok(());
            };
            if *serialized == "MaxItems" {
                let n = int_of(&v).map_err(Fail::Python)?;
                if let Value::Int(i) = n
                    && i <= BigInt::from(0)
                {
                    super::warn(
                        "warning: Non-positive values for --max-items may result in undefined behavior.\n",
                    );
                }
            }
            let entry = params
                .entry("PaginationConfig".to_string())
                .or_insert_with(Value::map);
            if let Value::Map(m) = entry {
                m.insert(serialized.to_string(), v);
            }
        }
        ArgKind::CliInput { .. } | ArgKind::Skeleton | ArgKind::Custom(_) => {}
        ArgKind::Outfile => {
            if let Some(Value::Str(s)) = value {
                state.outfile = Some(s);
            }
        }
        ArgKind::Count => {
            let Some(v) = value.filter(|v| !matches!(v, Value::Null)) else {
                return Ok(());
            };
            let count_error =
                || Fail::General("name 'ParamValidationError' is not defined".to_string());
            let Value::Str(s) = &v else {
                return Err(count_error());
            };
            let (min, max) = if s.contains(':') {
                let parts: Vec<&str> = s.split(':').collect();
                if parts.len() != 2 {
                    return Err(count_error());
                }
                (parts[0], parts[1])
            } else {
                (s.as_str(), s.as_str())
            };
            let min = int_of(&Value::str(min)).map_err(|_| count_error())?;
            let max = int_of(&Value::str(max)).map_err(|_| count_error())?;
            params.insert("MinCount".to_string(), min);
            params.insert("MaxCount".to_string(), max);
        }
        ArgKind::SecondaryIps => {
            if let Some(Value::List(items)) = value.as_ref().filter(|v| v.is_truthy()) {
                let ips: Vec<Value> = items
                    .iter()
                    .map(|v| {
                        let mut m = Map::new();
                        m.insert("PrivateIpAddress".to_string(), v.clone());
                        m.insert("Primary".to_string(), Value::Bool(false));
                        Value::Map(m)
                    })
                    .collect();
                build_network_interfaces(params, "PrivateIpAddresses", Value::List(ips));
            }
        }
        ArgKind::SecondaryIpCount => {
            if let Some(v) = value.filter(Value::is_truthy) {
                build_network_interfaces(params, "SecondaryPrivateIpAddressCount", v);
            }
        }
        ArgKind::AssociatePublicIp { negative } => {
            if value == Some(Value::Bool(!negative)) {
                build_network_interfaces(
                    params,
                    "AssociatePublicIpAddress",
                    Value::Bool(!negative),
                );
            }
        }
    }
    Ok(())
}

/// `runinstances._build_network_interfaces`.
fn build_network_interfaces(params: &mut Map, key: &str, value: Value) {
    let nis = params
        .entry("NetworkInterfaces".to_string())
        .or_insert_with(|| {
            let mut m = Map::new();
            m.insert("DeviceIndex".to_string(), Value::int(0));
            Value::List(vec![Value::Map(m)])
        });
    if let Value::List(l) = nis
        && let Some(Value::Map(first)) = l.first_mut()
    {
        if key == "PrivateIpAddresses" {
            if !first.contains_key("PrivateIpAddresses") {
                first.insert(key.to_string(), value);
            }
        } else {
            first.insert(key.to_string(), value);
        }
    }
}

/// `CliInputArgument.add_to_call_parameters`.
fn add_cli_input(ns: &IndexMap<String, NsVal>, yaml: bool, params: &mut Map) -> Result<(), Fail> {
    let (py, name) = if yaml {
        ("cli_input_yaml", "cli-input-yaml")
    } else {
        ("cli_input_json", "cli-input-json")
    };
    let Some(NsVal::Str(arg_value)) = ns.get(py) else {
        return Ok(());
    };
    let count = ns
        .iter()
        .filter(|(k, v)| !v.is_none() && k.starts_with("cli_input"))
        .count();
    if count != 1 {
        return Err(Fail::ParamValidation(
            "Only one --cli-input- parameter may be specified.".to_string(),
        ));
    }
    let text = match paramfile::get_paramfile(arg_value).map_err(|e| Fail::General(e.0))? {
        Some(Value::Str(s)) => s,
        Some(Value::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
        _ => arg_value.clone(),
    };
    let loaded = if yaml {
        load_yaml(&text).ok_or_else(|| param_error(name, "Invalid YAML received."))?
    } else {
        super::pyjson::loads(&text).map_err(|_| param_error(name, "Invalid JSON received."))?
    };
    let Value::Map(loaded) = loaded else {
        return Err(param_error(
            &format!("--{name}"),
            &format!(
                "Invalid type: expecting map, received <class '{}'>",
                super::shorthand::py_type_name(&loaded)
            ),
        ));
    };
    for (k, v) in loaded {
        if !params.contains_key(&k) {
            params.insert(k, v);
        }
    }
    Ok(())
}

/// `YAML(typ='safe', pure=True).load(text)`.
fn load_yaml(text: &str) -> Option<Value> {
    let v: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
    Some(yaml_value(&v))
}

fn yaml_value(v: &serde_yaml::Value) -> Value {
    match v {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::int(i)
            } else if let Some(u) = n.as_u64() {
                Value::Int(BigInt::from(u))
            } else {
                Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_yaml::Value::String(s) => Value::Str(s.clone()),
        serde_yaml::Value::Sequence(s) => Value::List(s.iter().map(yaml_value).collect()),
        serde_yaml::Value::Mapping(m) => Value::Map(
            m.iter()
                .map(|(k, v)| {
                    let key = match k {
                        serde_yaml::Value::String(s) => s.clone(),
                        other => crate::pyfmt::py_str(&yaml_value(other)),
                    };
                    (key, yaml_value(v))
                })
                .collect(),
        ),
        serde_yaml::Value::Tagged(t) => yaml_value(&t.value),
    }
}

/// `EC2PageSizeInjector.inject`.
fn inject_page_size(operation: &str, params: &mut Map) {
    if let Some(Value::Map(pc)) = params.get("PaginationConfig")
        && pc.contains_key("PageSize")
    {
        return;
    }
    let mut allowed: Vec<&str> = match operation {
        "describe-snapshots" => vec!["OwnerIds", "RestorableByUserIds"],
        _ => vec![],
    };
    allowed.extend(["NextToken", "DryRun", "PaginationConfig"]);
    if params.keys().any(|k| !allowed.contains(&k.as_str())) {
        return;
    }
    let entry = params
        .entry("PaginationConfig".to_string())
        .or_insert_with(Value::map);
    if let Value::Map(m) = entry {
        m.insert("PageSize".to_string(), Value::int(1000));
    }
}

/// `Paginator._extract_paging_params`.
fn pagination_config(pc: &Map) -> Result<PaginationConfig, Fail> {
    let int = |v: &Value| -> Result<i64, Fail> {
        match int_of(v).map_err(Fail::Python)? {
            Value::Int(i) => num_traits::ToPrimitive::to_i64(&i)
                .ok_or_else(|| Fail::General("int too large".to_string())),
            _ => unreachable!("int"),
        }
    };
    Ok(PaginationConfig {
        starting_token: pc.get("StartingToken").map(crate::pyfmt::py_str),
        max_items: pc.get("MaxItems").map(int).transpose()?,
        page_size: pc.get("PageSize").map(int).transpose()?,
    })
}

/// The steps of `BaseClient._make_api_call` between the client method and
/// validation that the CLI registers: `provide-client-params`
/// (`binaryformat.base64_decode_input_blobs`) and the CLI's
/// `before-parameter-build` handlers.
fn client_params(op: &Operation<'_>, params: &mut Value, ctx: &Ctx<'_>) -> Result<(), Fail> {
    if super::driver::binary_format(ctx) == "base64"
        && let Some(input) = op.input()
    {
        base64_decode(params, &input, true)?;
    }
    let Value::Map(m) = params else { return Ok(()) };
    if op.model.name == "ec2" {
        match op.name {
            "RunInstances" => {
                if !m.contains_key("MaxCount") {
                    m.insert("MaxCount".to_string(), Value::int(1));
                }
                if !m.contains_key("MinCount") {
                    m.insert("MinCount".to_string(), Value::int(1));
                }
                run_instances_fix_args(m);
            }
            "CreateNetworkAclEntry" | "ReplaceNetworkAclEntry" => {
                if let Some(Value::Str(p)) = m.get("Protocol") {
                    let fixed = match p.as_str() {
                        "tcp" => Some("6"),
                        "udp" => Some("17"),
                        "icmp" => Some("1"),
                        "all" => Some("-1"),
                        _ => None,
                    };
                    if let Some(f) = fixed {
                        m.insert("Protocol".to_string(), Value::str(f));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `runinstances._fix_args`.
fn run_instances_fix_args(params: &mut Map) {
    let keys = [
        "PrivateIpAddresses",
        "SecondaryPrivateIpAddressCount",
        "AssociatePublicIpAddress",
    ];
    let qualifies = matches!(
        params.get("NetworkInterfaces"),
        Some(Value::List(l)) if matches!(l.first(), Some(Value::Map(i)) if keys.iter().any(|k| i.contains_key(*k)))
    );
    if !qualifies {
        return;
    }
    let moves: [(&str, &str); 4] = [
        ("SubnetId", "SubnetId"),
        ("SecurityGroupIds", "Groups"),
        ("Ipv6AddressCount", "Ipv6AddressCount"),
        ("Ipv6Addresses", "Ipv6Addresses"),
    ];
    let mut taken: Vec<(String, Value)> = Vec::new();
    for (from, to) in &moves[..2] {
        if let Some(v) = params.shift_remove(*from) {
            taken.push((to.to_string(), v));
        }
    }
    if let Some(ip) = params.shift_remove("PrivateIpAddress") {
        let mut m = Map::new();
        m.insert("PrivateIpAddress".to_string(), ip);
        m.insert("Primary".to_string(), Value::Bool(true));
        taken.push((
            "PrivateIpAddresses".to_string(),
            Value::List(vec![Value::Map(m)]),
        ));
    }
    for (from, to) in &moves[2..] {
        if let Some(v) = params.shift_remove(*from) {
            taken.push((to.to_string(), v));
        }
    }
    if let Some(v) = params.shift_remove("EnablePrimaryIpv6") {
        taken.push(("PrimaryIpv6".to_string(), v));
    }
    if let Some(Value::List(l)) = params.get_mut("NetworkInterfaces")
        && let Some(Value::Map(interface)) = l.first_mut()
    {
        for (k, v) in taken {
            interface.insert(k, v);
        }
    }
}

/// `Base64DecodeVisitor` (a `ModelVisitor`).
fn base64_decode(value: &mut Value, shape: &Shape<'_>, root: bool) -> Result<(), Fail> {
    match shape.type_name() {
        "structure" => {
            if let Value::Map(m) = value {
                for (name, member) in shape.members() {
                    if let Some(v) = m.get_mut(name) {
                        base64_decode(v, &member, false)?;
                    }
                }
            }
        }
        "list" => {
            if let (Value::List(items), Some(member)) = (value, shape.list_member()) {
                for item in items {
                    base64_decode(item, &member, false)?;
                }
            }
        }
        "map" => {
            if let (Value::Map(m), Some(vs)) = (value, shape.map_value()) {
                for (_, v) in m.iter_mut() {
                    base64_decode(v, &vs, false)?;
                }
            }
        }
        "blob" => {
            if let Value::Str(s) = value
                && !root
            {
                if !s.is_ascii() {
                    return Err(Fail::Python(
                        "string argument should contain only ASCII characters".to_string(),
                    ));
                }
                match py_b64decode(s.as_bytes()) {
                    Some(b) => *value = Value::Bytes(b),
                    None => return Err(Fail::General(format!("Invalid base64: \"{s}\""))),
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// `base64.b64decode(s)` (`binascii.a2b_base64` without strict mode).
fn py_b64decode(data: &[u8]) -> Option<Vec<u8>> {
    fn sextet(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut quad = 0;
    let mut left: u32 = 0;
    let mut pads = 0;
    for &c in data {
        if c == b'=' {
            pads += 1;
            if quad >= 2 && quad + pads >= 4 {
                return Some(out);
            }
            continue;
        }
        let Some(v) = sextet(c) else { continue };
        let v = u32::from(v);
        pads = 0;
        match quad {
            0 => {
                quad = 1;
                left = v;
            }
            1 => {
                quad = 2;
                out.push(((left << 2) | (v >> 4)) as u8);
                left = v & 0x0f;
            }
            2 => {
                quad = 3;
                out.push(((left << 4) | (v >> 2)) as u8);
                left = v & 0x03;
            }
            _ => {
                quad = 0;
                out.push(((left << 6) | v) as u8);
                left = 0;
            }
        }
    }
    if quad != 0 { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64() {
        assert_eq!(py_b64decode(b"aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(py_b64decode(b"aGVs bG8"), None);
        assert_eq!(py_b64decode(b"aGVsbA=="), Some(b"hell".to_vec()));
        assert_eq!(py_b64decode(b"a"), None);
        assert_eq!(py_b64decode(b"!!aGk="), Some(b"hi".to_vec()));
    }
}
