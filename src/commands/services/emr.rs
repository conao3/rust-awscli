//! The `emr` customizations (`awscli/customizations/emr/`): the commands
//! `emr.register_commands` adds, and the `add-tags` / `list-clusters`
//! argument customizations.

mod config_writer;
mod create_cluster;
mod roles;
mod ssh;
mod steps;
pub(crate) mod util;

use super::basic::{self, Args};
use super::exec;
use crate::cli::argparse::{Kind, Nargs};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::{Map, Value};

use util::{CANCEL_AND_WAIT, HBASE_JAR_PATH, HBASE_MAIN, Step, param_error};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "add-tags" => exec::run_operation(ctx, "emr", name, args, &mut AddTags::new(ctx)),
        "list-clusters" => exec::run_operation(ctx, "emr", name, args, &mut ListClusters::new(ctx)),
        "terminate-clusters"
        | "describe-cluster"
        | "modify-cluster-attributes"
        | "install-applications"
        | "create-cluster"
        | "add-steps"
        | "restore-from-hbase-backup"
        | "create-hbase-backup"
        | "schedule-hbase-backup"
        | "disable-hbase-backups"
        | "create-default-roles"
        | "add-instance-groups"
        | "ssh"
        | "socks"
        | "get"
        | "put" => command(ctx, name, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `Command.__call__` / `Command._run_main` (`emr/command.py`).
fn command(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    let mut defs = util::arg_defs(name);
    util::override_required(ctx, name, &mut defs, args);
    let Some(mut parsed) = basic::parse(ctx, &["emr", name], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    util::apply_configs(ctx, name, &defs, &mut parsed)?;
    let region = util::region(ctx);
    if matches!(
        name,
        "install-applications"
            | "restore-from-hbase-backup"
            | "schedule-hbase-backup"
            | "create-hbase-backup"
            | "disable-hbase-backups"
    ) && let Some(cluster_id) = parsed.str("cluster_id")
        && let Some(label) = util::release_label(ctx, &cluster_id)?
        && !label.is_empty()
    {
        return Err(param_error(
            ctx,
            &format!("{name} is not supported with '{label}' release."),
        ));
    }
    let region = region.as_deref();
    match name {
        "terminate-clusters" => {
            let mut p = Map::new();
            p.insert(
                "JobFlowIds".to_string(),
                parsed.get("cluster_ids").cloned().unwrap_or_default(),
            );
            util::call_and_display(ctx, "TerminateJobFlows", Value::Map(p))?;
        }
        "describe-cluster" => describe_cluster(ctx, &parsed)?,
        "modify-cluster-attributes" => modify_cluster_attributes(ctx, &parsed)?,
        "install-applications" => install_applications(ctx, &parsed, region)?,
        "create-cluster" => create_cluster::run(ctx, &mut parsed, region)?,
        "add-steps" => add_steps(ctx, &parsed, region)?,
        "restore-from-hbase-backup" => {
            let mut a = vec![
                HBASE_MAIN.to_string(),
                "--restore".to_string(),
                "--backup-dir".to_string(),
                parsed.str("dir").unwrap_or_default(),
            ];
            if let Some(v) = parsed.str("backup_version") {
                a.push("--backup-version".to_string());
                a.push(v);
            }
            add_hbase_step(ctx, &parsed, "Restore HBase", a)?;
        }
        "create-hbase-backup" => {
            let mut a = vec![
                HBASE_MAIN.to_string(),
                "--backup".to_string(),
                "--backup-dir".to_string(),
                parsed.str("dir").unwrap_or_default(),
            ];
            if parsed.get("consistent") == Some(&Value::Bool(true)) {
                a.push("--consistent".to_string());
            }
            add_hbase_step(ctx, &parsed, "Backup HBase", a)?;
        }
        "schedule-hbase-backup" => schedule_hbase_backup(ctx, &parsed)?,
        "disable-hbase-backups" => {
            let mut a = vec![
                HBASE_MAIN.to_string(),
                "--set-scheduled-backup".to_string(),
                "false".to_string(),
            ];
            let full = parsed.get("full") == Some(&Value::Bool(true));
            let incremental = parsed.get("incremental") == Some(&Value::Bool(true));
            if !full && !incremental {
                return Err(param_error(
                    ctx,
                    "Should specify at least one of --full and --incremental.",
                ));
            }
            if full {
                a.push("--disable-full-backups".to_string());
            }
            if incremental {
                a.push("--disable-incremental-backups".to_string());
            }
            add_hbase_step(ctx, &parsed, "Modify Backup Schedule", a)?;
        }
        "create-default-roles" => roles::create_default_roles(ctx, &parsed, region)?,
        "add-instance-groups" => add_instance_groups(ctx, &parsed)?,
        "ssh" | "socks" | "get" | "put" => return ssh::run(ctx, name, &parsed),
        _ => unreachable!("emr command"),
    }
    Ok(ExitCode::Success)
}

fn cluster_param(parsed: &Args, key: &str) -> Map {
    let mut p = Map::new();
    p.insert(
        key.to_string(),
        parsed.get("cluster_id").cloned().unwrap_or_default(),
    );
    p
}

fn add_hbase_step(
    ctx: &Context,
    parsed: &Args,
    name: &str,
    args: Vec<String>,
) -> Result<(), CliError> {
    let step = util::build_step(ctx, Step::new(HBASE_JAR_PATH, name, CANCEL_AND_WAIT, args))?;
    let mut p = cluster_param(parsed, "JobFlowId");
    p.insert("Steps".to_string(), Value::List(vec![step]));
    util::call_and_display(ctx, "AddJobFlowSteps", Value::Map(p))
}

/// `ScheduleHBaseBackup._run_main_command`.
fn schedule_hbase_backup(ctx: &Context, parsed: &Args) -> Result<(), CliError> {
    let t = parsed.str("type").unwrap_or_default().to_lowercase();
    if t != "full" && t != "incremental" {
        return Err(param_error(
            ctx,
            "invalid type. type should be either full or incremental.",
        ));
    }
    let unit = parsed.str("unit").unwrap_or_default().to_lowercase();
    if !["minutes", "hours", "days"].contains(&unit.as_str()) {
        return Err(param_error(
            ctx,
            "invalid unit. unit should be one of the following values: minutes, hours or days.",
        ));
    }
    let full = t == "full";
    let mut a = vec![
        HBASE_MAIN.to_string(),
        "--set-scheduled-backup".to_string(),
        "true".to_string(),
        "--backup-dir".to_string(),
        parsed.str("dir").unwrap_or_default(),
    ];
    if parsed.get("consistent") == Some(&Value::Bool(true)) {
        a.push("--consistent".to_string());
    }
    a.push(
        if full {
            "--full-backup-time-interval"
        } else {
            "--incremental-backup-time-interval"
        }
        .to_string(),
    );
    a.push(parsed.str("interval").unwrap_or_default());
    a.push(
        if full {
            "--full-backup-time-unit"
        } else {
            "--incremental-backup-time-unit"
        }
        .to_string(),
    );
    a.push(unit);
    a.push("--start-time".to_string());
    a.push(parsed.str("start_time").unwrap_or("now".to_string()));
    add_hbase_step(ctx, parsed, "Modify Backup Schedule", a)
}

/// `DescribeCluster._run_main_command`.
fn describe_cluster(ctx: &Context, parsed: &Args) -> Result<(), CliError> {
    let client = util::emr_client(ctx)?;
    let params = Value::Map(cluster_param(parsed, "ClusterId"));
    let mut result = client.call("DescribeCluster", &params)?;
    let fleets = result
        .get("Cluster")
        .and_then(|c| c.get("InstanceCollectionType"))
        .and_then(Value::as_str)
        == Some("INSTANCE_FLEET");
    let (fleet_result, group_result) = if fleets {
        (Some(client.call("ListInstanceFleets", &params)?), None)
    } else {
        (None, Some(client.call("ListInstanceGroups", &params)?))
    };
    let bootstrap = client.call("ListBootstrapActions", &params)?;
    let Some(cluster) = result
        .as_map_mut()
        .and_then(|m| m.get_mut("Cluster"))
        .and_then(Value::as_map_mut)
    else {
        return Err(util::general_error(ctx, "'Cluster'"));
    };
    cluster.insert("BootstrapActions".to_string(), Value::List(Vec::new()));
    if let Some(v) = fleet_result
        .as_ref()
        .and_then(|r| r.get("InstanceFleets"))
        .filter(|v| !matches!(v, Value::Null))
    {
        cluster.insert("InstanceFleets".to_string(), v.clone());
    }
    if let Some(v) = group_result
        .as_ref()
        .and_then(|r| r.get("InstanceGroups"))
        .filter(|v| !matches!(v, Value::Null))
    {
        cluster.insert("InstanceGroups".to_string(), v.clone());
    }
    if let Some(v) = bootstrap
        .get("BootstrapActions")
        .filter(|v| !matches!(v, Value::Null))
    {
        cluster.insert("BootstrapActions".to_string(), v.clone());
    }
    util::display(ctx, "describe_cluster", result)
}

/// `ModifyClusterAttr._run_main_command`.
fn modify_cluster_attributes(ctx: &Context, parsed: &Args) -> Result<(), CliError> {
    let b = |k: &str| parsed.bool(k);
    for (on, off) in [
        ("visible_to_all_users", "no_visible_to_all_users"),
        ("termination_protected", "no_termination_protected"),
        ("auto_terminate", "no_auto_terminate"),
        (
            "unhealthy_node_replacement",
            "no_unhealthy_node_replacement",
        ),
    ] {
        if b(on) && b(off) {
            return Err(util::mutual_exclusive(
                ctx,
                &format!("--{}", on.replace('_', "-")),
                &format!("--{}", off.replace('_', "-")),
                "",
            ));
        }
    }
    let any = [
        "termination_protected",
        "no_termination_protected",
        "visible_to_all_users",
        "no_visible_to_all_users",
        "auto_terminate",
        "no_auto_terminate",
        "unhealthy_node_replacement",
        "no_unhealthy_node_replacement",
    ]
    .iter()
    .any(|k| b(k));
    if !any {
        return Err(param_error(
            ctx,
            "Must specify one of the following boolean options: --visible-to-all-users|--no-visible-to-all-users, --termination-protected|--no-termination-protected, --auto-terminate|--no-auto-terminate, --unhealthy-node-replacement|--no-unhealthy-node-replacement.",
        ));
    }
    let ids = Value::List(vec![parsed.get("cluster_id").cloned().unwrap_or_default()]);
    let call = |op: &str, key: &str, v: bool| {
        let mut p = Map::new();
        p.insert("JobFlowIds".to_string(), ids.clone());
        p.insert(key.to_string(), Value::Bool(v));
        util::call_and_display(ctx, op, Value::Map(p))
    };
    if b("visible_to_all_users") || b("no_visible_to_all_users") {
        call(
            "SetVisibleToAllUsers",
            "VisibleToAllUsers",
            b("visible_to_all_users") && !b("no_visible_to_all_users"),
        )?;
    }
    if b("termination_protected") || b("no_termination_protected") {
        call(
            "SetTerminationProtection",
            "TerminationProtected",
            b("termination_protected") && !b("no_termination_protected"),
        )?;
    }
    if b("auto_terminate") || b("no_auto_terminate") {
        call(
            "SetKeepJobFlowAliveWhenNoSteps",
            "KeepJobFlowAliveWhenNoSteps",
            !(b("auto_terminate") && !b("no_auto_terminate")),
        )?;
    }
    if b("unhealthy_node_replacement") || b("no_unhealthy_node_replacement") {
        call(
            "SetUnhealthyNodeReplacement",
            "UnhealthyNodeReplacement",
            b("unhealthy_node_replacement") && !b("no_unhealthy_node_replacement"),
        )?;
    }
    Ok(())
}

/// `InstallApplications._run_main_command`.
fn install_applications(
    ctx: &Context,
    parsed: &Args,
    region: Option<&str>,
) -> Result<(), CliError> {
    let apps = parsed.list("applications").unwrap_or_default();
    for app in &apps {
        let name = app.get("Name").and_then(Value::as_str).unwrap_or_default();
        let upper = name.to_uppercase();
        if util::APPLICATIONS.contains(&upper.as_str()) {
            if upper != "HIVE" && upper != "PIG" {
                return Err(param_error(
                    ctx,
                    &format!(
                        "{name} cannot be installed on a running cluster. 'Name' should be one of the following: HIVE, PIG"
                    ),
                ));
            }
        } else {
            return Err(param_error(
                ctx,
                &format!(
                    "Unknown application: {name}. 'Name' should be one of the following: {}",
                    util::APPLICATIONS.join(", ")
                ),
            ));
        }
    }
    let (_, _, step_list) = steps::build_applications(ctx, region, &apps, None)?;
    let mut p = cluster_param(parsed, "JobFlowId");
    p.insert("Steps".to_string(), Value::List(step_list));
    util::call_and_display(ctx, "AddJobFlowSteps", Value::Map(p))
}

/// `AddSteps._run_main_command`.
fn add_steps(ctx: &Context, parsed: &Args, region: Option<&str>) -> Result<(), CliError> {
    let cluster_id = parsed.str("cluster_id").unwrap_or_default();
    let release_label = util::release_label(ctx, &cluster_id)?;
    let step_list = steps::build_step_config_list(
        ctx,
        &parsed.list("steps").unwrap_or_default(),
        region,
        release_label.as_deref(),
    )?;
    let mut p = cluster_param(parsed, "JobFlowId");
    p.insert("Steps".to_string(), Value::List(step_list));
    if let Some(arn) = parsed.get("execution_role_arn") {
        p.insert("ExecutionRoleArn".to_string(), arn.clone());
    }
    util::call_and_display(ctx, "AddJobFlowSteps", Value::Map(p))
}

/// `AddInstanceGroups._run_main_command`.
fn add_instance_groups(ctx: &Context, parsed: &Args) -> Result<(), CliError> {
    let mut p = cluster_param(parsed, "JobFlowId");
    p.insert(
        "InstanceGroups".to_string(),
        Value::List(create_cluster::build_instance_groups(
            &parsed.list("instance_groups").unwrap_or_default(),
        )),
    );
    let client = util::emr_client(ctx)?;
    let result = client.call("AddInstanceGroups", &Value::Map(p))?;
    let mut out = Map::new();
    let id = result
        .get("JobFlowId")
        .filter(|v| !matches!(v, Value::Null));
    let groups = result
        .get("InstanceGroupIds")
        .filter(|v| !matches!(v, Value::Null));
    if let (Some(id), Some(groups)) = (id, groups) {
        out.insert("ClusterId".to_string(), id.clone());
        out.insert("InstanceGroupIds".to_string(), groups.clone());
        out.insert(
            "ClusterArn".to_string(),
            result.get("ClusterArn").cloned().unwrap_or_default(),
        );
    }
    util::display(ctx, "add_instance_groups", Value::Map(out))
}

/// The `emr` configuration keys (`override_args_required_option` for the
/// operations).
fn config_keys(ctx: &Context) -> Vec<String> {
    util::emr_configs(ctx).keys().cloned().collect()
}

fn override_required_table(keys: &[String], table: &mut ArgTable<'_>) {
    crate::cli::driver::override_emr_required(keys, table);
}

/// `addtags.modify_tags_argument`.
struct AddTags {
    keys: Vec<String>,
}

impl AddTags {
    fn new(ctx: &Context) -> AddTags {
        AddTags {
            keys: config_keys(ctx),
        }
    }
}

impl OperationHooks for AddTags {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        table.insert(
            "tags".to_string(),
            Arg::custom_argument(
                "tags",
                CustomSpec {
                    nargs: Some(Nargs::OneOrMore),
                    ..CustomSpec::default()
                },
                true,
            ),
        );
        override_required_table(&self.keys, table);
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        if name == "tags"
            && let Some(v) = value
        {
            let tags: Vec<String> = match v {
                Value::List(l) => l.iter().map(crate::pyfmt::py_str).collect(),
                Value::Str(s) => s.chars().map(|c| c.to_string()).collect(),
                _ => Vec::new(),
            };
            params.insert("Tags".to_string(), util::parse_tags(&tags));
        }
        Ok(())
    }
}

/// `listclusters.modify_list_clusters_argument`.
struct ListClusters {
    keys: Vec<String>,
}

impl ListClusters {
    fn new(ctx: &Context) -> ListClusters {
        ListClusters {
            keys: config_keys(ctx),
        }
    }
}

const STATES_ERROR: &str = "You can specify only one of the cluster state filters: --cluster-states, --active, --terminated, --failed.";

impl OperationHooks for ListClusters {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        let flag = || CustomSpec {
            action: Some(Kind::StoreTrue),
            ..CustomSpec::default()
        };
        table.insert(
            "cluster-states".to_string(),
            Arg::custom_argument(
                "cluster-states",
                CustomSpec {
                    nargs: Some(Nargs::OneOrMore),
                    ..CustomSpec::default()
                },
                false,
            ),
        );
        for name in ["active", "terminated", "failed"] {
            table.insert(name.to_string(), Arg::custom_argument(name, flag(), false));
        }
        for name in ["created-before", "created-after"] {
            table.insert(
                name.to_string(),
                Arg::custom_argument(name, CustomSpec::default(), false),
            );
        }
        override_required_table(&self.keys, table);
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let has_states = || {
            params.get("ClusterStates").is_some_and(|v| {
                !matches!(v, Value::Null) && v.as_list().is_none_or(|l| !l.is_empty())
            })
        };
        let states = |list: &[&str]| Value::List(list.iter().map(|s| Value::str(*s)).collect());
        match name {
            "cluster-states" => {
                if let Some(v) = value {
                    if has_states() {
                        return Err(Fail::ParamValidation(STATES_ERROR.to_string()));
                    }
                    params.insert("ClusterStates".to_string(), v);
                }
            }
            "active" | "terminated" | "failed" => {
                if value == Some(Value::Bool(true)) {
                    if has_states() {
                        return Err(Fail::ParamValidation(STATES_ERROR.to_string()));
                    }
                    let list: &[&str] = match name {
                        "active" => &[
                            "STARTING",
                            "BOOTSTRAPPING",
                            "RUNNING",
                            "WAITING",
                            "TERMINATING",
                        ],
                        "terminated" => &["TERMINATED"],
                        _ => &["TERMINATED_WITH_ERRORS"],
                    };
                    params.insert("ClusterStates".to_string(), states(list));
                }
            }
            "created-before" => {
                if let Some(v) = value {
                    params.insert("CreatedBefore".to_string(), v);
                }
            }
            "created-after" => {
                if let Some(v) = value {
                    params.insert("CreatedAfter".to_string(), v);
                }
            }
            _ => {}
        }
        Ok(())
    }
}
