//! `emr create-cluster` (`awscli/customizations/emr/createcluster.py`,
//! `instancegroupsutils.py`, `instancefleetsutils.py`, `emrfsutils.py`).

use super::super::basic::Args;
use super::steps;
use super::util::{
    self, CANCEL_AND_WAIT, COMMAND_RUNNER, EC2_ROLE_NAME, EMR_ROLE_NAME, HBASE_JAR_PATH,
    HBASE_MAIN, Step, TERMINATE_CLUSTER, apply, apply_boolean_options, apply_params, general_error,
    mutual_exclusive, param_error,
};
use crate::driver::Context;
use crate::error::CliError;
use crate::value::{Map, Value};

fn list_of(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::List(l)) => l.clone(),
        _ => Vec::new(),
    }
}

fn map_of(v: &Value) -> Map {
    v.as_map().cloned().unwrap_or_default()
}

/// `instancegroupsutils.build_instance_groups`.
pub(crate) fn build_instance_groups(groups: &[Value]) -> Vec<Value> {
    groups
        .iter()
        .map(|g| {
            let g = map_of(g);
            let mut ig = Map::new();
            let group_type = g.get("InstanceGroupType").cloned().unwrap_or_default();
            ig.insert(
                "Name".to_string(),
                g.get("Name").cloned().unwrap_or_else(|| group_type.clone()),
            );
            ig.insert(
                "InstanceType".to_string(),
                g.get("InstanceType").cloned().unwrap_or_default(),
            );
            ig.insert(
                "InstanceCount".to_string(),
                g.get("InstanceCount").cloned().unwrap_or_default(),
            );
            ig.insert(
                "InstanceRole".to_string(),
                Value::str(group_type.as_str().unwrap_or_default().to_uppercase()),
            );
            if let Some(bid) = g.get("BidPrice") {
                if bid.as_str() != Some("OnDemandPrice") {
                    ig.insert("BidPrice".to_string(), bid.clone());
                }
                ig.insert("Market".to_string(), Value::str("SPOT"));
            } else {
                ig.insert("Market".to_string(), Value::str("ON_DEMAND"));
            }
            for key in [
                "EbsConfiguration",
                "AutoScalingPolicy",
                "Configurations",
                "CustomAmiId",
            ] {
                if let Some(v) = g.get(key) {
                    ig.insert(key.to_string(), v.clone());
                }
            }
            Value::Map(ig)
        })
        .collect()
}

fn simple_group(instance_type: &str, count: i64, role: &str) -> Value {
    let mut ig = Map::new();
    ig.insert("InstanceType".to_string(), Value::str(instance_type));
    ig.insert("InstanceCount".to_string(), Value::int(count));
    ig.insert("InstanceRole".to_string(), Value::str(role));
    ig.insert("Name".to_string(), Value::str(role));
    ig.insert("Market".to_string(), Value::str("ON_DEMAND"));
    Value::Map(ig)
}

/// `int(s)` for the `--instance-count` string.
fn py_int(ctx: &Context, s: &str) -> Result<i64, CliError> {
    s.trim()
        .replace('_', "")
        .parse::<i64>()
        .ok()
        .filter(|_| !s.trim().starts_with('_') && !s.trim().ends_with('_') && !s.contains("__"))
        .ok_or_else(|| {
            general_error(
                ctx,
                &format!(
                    "invalid literal for int() with base 10: {}",
                    crate::pyfmt::str_repr(s)
                ),
            )
        })
}

/// `instancegroupsutils.validate_and_build_instance_groups`.
fn validate_and_build_instance_groups(
    ctx: &Context,
    groups: Option<&Value>,
    instance_type: Option<&str>,
    instance_count: Option<&str>,
) -> Result<Vec<Value>, CliError> {
    if groups.is_none() && instance_type.is_none() {
        return Err(param_error(
            ctx,
            "Must specify either --instance-groups or --instance-type with --instance-count(optional) to configure instance groups.",
        ));
    }
    if groups.is_some() && (instance_type.is_some() || instance_count.is_some()) {
        return Err(param_error(
            ctx,
            "You may not specify --instance-type or --instance-count with --instance-groups, because --instance-type and --instance-count are shortcut options for --instance-groups.",
        ));
    }
    if let Some(g) = groups {
        return Ok(build_instance_groups(&list_of(Some(g))));
    }
    let instance_type = instance_type.unwrap_or_default();
    let mut out = vec![simple_group(instance_type, 1, "MASTER")];
    if let Some(c) = instance_count {
        let n = py_int(ctx, c)?;
        if n > 1 {
            out.push(simple_group(instance_type, n - 1, "CORE"));
        }
    }
    Ok(out)
}

/// `instancefleetsutils.validate_and_build_instance_fleets`.
fn build_instance_fleets(fleets: &[Value]) -> Vec<Value> {
    fleets
        .iter()
        .map(|f| {
            let f = map_of(f);
            let mut c = Map::new();
            let fleet_type = f.get("InstanceFleetType").cloned().unwrap_or_default();
            c.insert(
                "Name".to_string(),
                f.get("Name").cloned().unwrap_or_else(|| fleet_type.clone()),
            );
            c.insert("InstanceFleetType".to_string(), fleet_type);
            for key in [
                "TargetOnDemandCapacity",
                "TargetSpotCapacity",
                "InstanceTypeConfigs",
            ] {
                if let Some(v) = f.get(key) {
                    c.insert(key.to_string(), v.clone());
                }
            }
            for (key, subs) in [
                (
                    "LaunchSpecifications",
                    ["SpotSpecification", "OnDemandSpecification"],
                ),
                (
                    "ResizeSpecifications",
                    ["SpotResizeSpecification", "OnDemandResizeSpecification"],
                ),
            ] {
                if let Some(spec) = f.get(key) {
                    let spec = map_of(spec);
                    let mut out = Map::new();
                    for sub in subs {
                        if let Some(v) = spec.get(sub) {
                            out.insert(sub.to_string(), v.clone());
                        }
                    }
                    c.insert(key.to_string(), Value::Map(out));
                }
            }
            if let Some(v) = f.get("Context") {
                c.insert("Context".to_string(), v.clone());
            }
            Value::Map(c)
        })
        .collect()
}

/// `CreateCluster._update_cluster_dict`.
fn update(cluster: &mut Map, key: &str, value: Vec<Value>) {
    if let Some(Value::List(existing)) = cluster.get_mut(key) {
        existing.extend(value);
    } else if !value.is_empty() {
        cluster.insert(key.to_string(), Value::List(value));
    }
}

fn s(args: &Args, key: &str) -> Option<String> {
    args.str(key)
}

pub(crate) fn run(ctx: &Context, args: &mut Args, region: Option<&str>) -> Result<(), CliError> {
    let mut params = Map::new();
    params.insert(
        "Name".to_string(),
        args.get("name").cloned().unwrap_or_default(),
    );
    let release_label = s(args, "release_label");
    let ami_version = s(args, "ami_version");
    if ami_version.is_some() && release_label.is_some() {
        return Err(mutual_exclusive(
            ctx,
            "--ami-version",
            "--release-label",
            "",
        ));
    }
    if ami_version.is_none() && release_label.is_none() {
        return Err(param_error(
            ctx,
            "Either --ami-version or --release-label is required.",
        ));
    }
    let role_message = " Either choose --use-default-roles or use both --service-role <roleName> and --ec2-attributes InstanceProfile=<profileName>.";
    let use_default_roles = args.get("use_default_roles") == Some(&Value::Bool(true));
    if use_default_roles && args.get("service_role").is_some() {
        return Err(mutual_exclusive(
            ctx,
            "--use-default-roles",
            "--service-role",
            role_message,
        ));
    }
    if use_default_roles
        && args
            .get("ec2_attributes")
            .and_then(Value::as_map)
            .is_some_and(|m| m.contains_key("InstanceProfile"))
    {
        return Err(mutual_exclusive(
            ctx,
            "--use-default-roles",
            "--ec2-attributes InstanceProfile",
            role_message,
        ));
    }
    let groups = args.get("instance_groups").cloned();
    let fleets = args.get("instance_fleets").cloned();
    if groups.is_some() && fleets.is_some() {
        return Err(mutual_exclusive(
            ctx,
            "--instance-groups",
            "--instance-fleets",
            "",
        ));
    }
    let mut instances = Map::new();
    if let Some(f) = &fleets {
        instances.insert(
            "InstanceFleets".to_string(),
            Value::List(build_instance_fleets(&list_of(Some(f)))),
        );
    } else {
        instances.insert(
            "InstanceGroups".to_string(),
            Value::List(validate_and_build_instance_groups(
                ctx,
                groups.as_ref(),
                s(args, "instance_type").as_deref(),
                s(args, "instance_count").as_deref(),
            )?),
        );
    }
    if let Some(label) = &release_label {
        params.insert("ReleaseLabel".to_string(), Value::str(label.clone()));
        if let Some(c) = s(args, "configurations") {
            let parsed = crate::cli::pyjson::loads(&c).map_err(|_| {
                param_error(ctx, "invalid json argument for option --configurations")
            })?;
            params.insert("Configurations".to_string(), parsed);
        }
    }
    if release_label.is_none()
        && let Some(ami) = &ami_version
    {
        let valid = {
            let mut chars = ami.chars();
            let first = chars.next();
            match first {
                Some('.') => true,
                Some(c) if c.is_ascii_digit() => chars.next() == Some('.'),
                _ => false,
            }
        };
        if !valid {
            return Err(param_error(
                ctx,
                &format!(
                    "The supplied AMI version \"{ami}\" is invalid. Please see AMI Versions Supported in Amazon EMR in Amazon Elastic MapReduce Developer Guide: http://docs.aws.amazon.com/ElasticMapReduce/latest/DeveloperGuide/ami-versions-supported.html"
                ),
            ));
        }
        params.insert("AmiVersion".to_string(), Value::str(ami.clone()));
    }
    apply(
        &mut params,
        "AdditionalInfo",
        args.get("additional_info").cloned(),
    );
    apply(&mut params, "LogUri", args.get("log_uri").cloned());
    apply(
        &mut params,
        "OSReleaseLabel",
        args.get("os_release_label").cloned(),
    );
    apply(
        &mut params,
        "LogEncryptionKmsKeyId",
        args.get("log_encryption_kms_key_id").cloned(),
    );
    if use_default_roles {
        args.values
            .insert("service_role".to_string(), Value::str(EMR_ROLE_NAME));
        if args.get("ec2_attributes").is_none() {
            args.values
                .insert("ec2_attributes".to_string(), Value::map());
        }
        if let Some(Value::Map(m)) = args.values.get_mut("ec2_attributes") {
            m.insert("InstanceProfile".to_string(), Value::str(EC2_ROLE_NAME));
        }
    }
    apply(
        &mut params,
        "ServiceRole",
        args.get("service_role").cloned(),
    );
    if groups.is_some()
        && let Some(Value::List(igs)) = instances.get("InstanceGroups")
        && igs.iter().any(|g| g.get("AutoScalingPolicy").is_some())
        && args.get("auto_scaling_role").is_none()
    {
        return Err(param_error(
            ctx,
            "Must specify --auto-scaling-role when configuring an AutoScaling policy for an instance group.",
        ));
    }
    apply(
        &mut params,
        "AutoScalingRole",
        args.get("auto_scaling_role").cloned(),
    );
    apply(
        &mut params,
        "ScaleDownBehavior",
        args.get("scale_down_behavior").cloned(),
    );

    let b = |k: &str| args.bool(k);
    let mut no_auto_terminate = b("no_auto_terminate");
    if !no_auto_terminate && !b("auto_terminate") {
        no_auto_terminate = true;
    }
    instances.insert(
        "KeepJobFlowAliveWhenNoSteps".to_string(),
        Value::Bool(apply_boolean_options(
            ctx,
            no_auto_terminate,
            "--no-auto-terminate",
            b("auto_terminate"),
            "--auto-terminate",
        )?),
    );
    instances.insert(
        "TerminationProtected".to_string(),
        Value::Bool(apply_boolean_options(
            ctx,
            b("termination_protected"),
            "--termination-protected",
            b("no_termination_protected"),
            "--no-termination-protected",
        )?),
    );
    if b("unhealthy_node_replacement") || b("no_unhealthy_node_replacement") {
        instances.insert(
            "UnhealthyNodeReplacement".to_string(),
            Value::Bool(apply_boolean_options(
                ctx,
                b("unhealthy_node_replacement"),
                "--unhealthy-node-replacement",
                b("no_unhealthy_node_replacement"),
                "--no-unhealthy-node-replacement",
            )?),
        );
    }
    let mut visible = b("visible_to_all_users");
    if !visible && !b("no_visible_to_all_users") {
        visible = true;
    }
    params.insert(
        "VisibleToAllUsers".to_string(),
        Value::Bool(apply_boolean_options(
            ctx,
            visible,
            "--visible-to-all-users",
            b("no_visible_to_all_users"),
            "--no-visible-to-all-users",
        )?),
    );
    params.insert(
        "Tags".to_string(),
        util::parse_tags(&args.str_list("tags").unwrap_or_default()),
    );
    params.insert("Instances".to_string(), Value::Map(instances));
    if let Some(attrs) = args.get("ec2_attributes").cloned() {
        build_ec2_attributes(ctx, &mut params, &map_of(&attrs))?;
    }
    let debugging = apply_boolean_options(
        ctx,
        b("enable_debugging"),
        "--enable-debugging",
        b("no_enable_debugging"),
        "--no-enable-debugging",
    )?;
    if args.get("log_uri").is_none() && debugging {
        return Err(param_error(
            ctx,
            "LogUri not specified. You must specify a logUri if you enable debugging when creating a cluster.",
        ));
    }
    if debugging {
        let (jar, step_args) = if release_label.as_deref().is_some_and(|l| !l.is_empty()) {
            (
                COMMAND_RUNNER.to_string(),
                vec!["state-pusher-script".to_string()],
            )
        } else {
            (
                util::script_runner(region),
                vec![util::s3_link("/libs/state-pusher/0.1/fetch", region)],
            )
        };
        let step = util::build_step(
            ctx,
            Step::new(&jar, "Setup Hadoop Debugging", TERMINATE_CLUSTER, step_args),
        )?;
        update(&mut params, "Steps", vec![step]);
    }
    if let Some(apps) = args.get("applications").cloned() {
        let apps = list_of(Some(&apps));
        if release_label.is_none() {
            let ami = params
                .get("AmiVersion")
                .and_then(Value::as_str)
                .map(str::to_string);
            let (app_list, ba_list, step_list) =
                steps::build_applications(ctx, region, &apps, ami.as_deref())?;
            update(&mut params, "NewSupportedProducts", app_list);
            update(&mut params, "BootstrapActions", ba_list);
            update(&mut params, "Steps", step_list);
        } else {
            params.insert("Applications".to_string(), Value::List(apps));
        }
    }
    if let Some(restore) = args.get("restore_from_hbase_backup").cloned() {
        let mut step_args = vec![
            HBASE_MAIN.to_string(),
            "--restore".to_string(),
            "--backup-dir".to_string(),
            restore
                .get("Dir")
                .map(crate::pyfmt::py_str)
                .unwrap_or("None".to_string()),
        ];
        if let Some(v) = restore.get("BackupVersion") {
            step_args.push("--backup-version".to_string());
            step_args.push(crate::pyfmt::py_str(v));
        }
        let step = util::build_step(
            ctx,
            Step::new(HBASE_JAR_PATH, "Restore HBase", CANCEL_AND_WAIT, step_args),
        )?;
        update(&mut params, "Steps", vec![step]);
    }
    if let Some(bas) = args.get("bootstrap_actions").cloned() {
        build_bootstrap_actions(ctx, &mut params, &list_of(Some(&bas)))?;
    }
    if let Some(emrfs) = args.get("emrfs").cloned() {
        let emrfs = map_of(&emrfs);
        if release_label.as_deref().is_some_and(|l| !l.is_empty()) {
            if let Some(Value::List(configs)) = params.get("Configurations")
                && configs
                    .iter()
                    .any(|c| c.get("Classification").and_then(Value::as_str) == Some("emrfs-site"))
            {
                return Err(param_error(
                    ctx,
                    "EMRFS should be configured either using --configuration or --emrfs but not both",
                ));
            }
            let config = emrfs_configuration(ctx, &emrfs)?;
            update(&mut params, "Configurations", vec![config]);
        } else {
            let bas = emrfs_bootstrap_actions(ctx, region, &emrfs)?;
            update(&mut params, "BootstrapActions", bas);
        }
    }
    if let Some(step_list) = args.get("steps").cloned() {
        let built = steps::build_step_config_list(
            ctx,
            &list_of(Some(&step_list)),
            region,
            release_label.as_deref(),
        )?;
        update(&mut params, "Steps", built);
    }
    apply(
        &mut params,
        "SecurityConfiguration",
        args.get("security_configuration").cloned(),
    );
    apply(
        &mut params,
        "CustomAmiId",
        args.get("custom_ami_id").cloned(),
    );
    for (arg, key) in [
        ("ebs_root_volume_size", "EbsRootVolumeSize"),
        ("ebs_root_volume_iops", "EbsRootVolumeIops"),
        ("ebs_root_volume_throughput", "EbsRootVolumeThroughput"),
    ] {
        if let Some(v) = s(args, arg) {
            apply(&mut params, key, Some(Value::int(py_int(ctx, &v)?)));
        }
    }
    apply(
        &mut params,
        "RepoUpgradeOnBoot",
        args.get("repo_upgrade_on_boot").cloned(),
    );
    apply(
        &mut params,
        "KerberosAttributes",
        args.get("kerberos_attributes").cloned(),
    );
    if let Some(v) = args.get("step_concurrency_level") {
        params.insert("StepConcurrencyLevel".to_string(), v.clone());
    }
    apply(
        &mut params,
        "StepExecutionRoleArn",
        args.get("step_execution_role_arn").cloned(),
    );
    for (on, off, key) in [
        ("extended_support", "no_extended_support", "ExtendedSupport"),
        ("session_enabled", "no_session_enabled", "SessionEnabled"),
    ] {
        if b(on) || b(off) {
            params.insert(
                key.to_string(),
                Value::Bool(apply_boolean_options(
                    ctx,
                    b(on),
                    &format!("--{}", on.replace('_', "-")),
                    b(off),
                    &format!("--{}", off.replace('_', "-")),
                )?),
            );
        }
    }
    apply(
        &mut params,
        "ManagedScalingPolicy",
        args.get("managed_scaling_policy").cloned(),
    );
    apply(
        &mut params,
        "PlacementGroupConfigs",
        args.get("placement_group_configs").cloned(),
    );
    apply(
        &mut params,
        "AutoTerminationPolicy",
        args.get("auto_termination_policy").cloned(),
    );
    if let Some(mc) = args.get("monitoring_configuration").cloned() {
        apply(&mut params, "MonitoringConfiguration", Some(mc.clone()));
        validate_s3_logging_configuration(ctx, &mc, s(args, "log_uri").as_deref())?;
    }
    validate_required_applications(ctx, args)?;

    let result = util::call(ctx, "RunJobFlow", &Value::Map(params))?;
    let mut out = Map::new();
    if let Some(id) = result
        .get("JobFlowId")
        .filter(|v| !matches!(v, Value::Null))
    {
        out.insert("ClusterId".to_string(), id.clone());
        out.insert(
            "ClusterArn".to_string(),
            result.get("ClusterArn").cloned().unwrap_or_default(),
        );
    }
    util::display(ctx, "run_job_flow", Value::Map(out))
}

/// `CreateCluster._build_ec2_attributes`.
fn build_ec2_attributes(ctx: &Context, cluster: &mut Map, attrs: &Map) -> Result<(), CliError> {
    let has = |k: &str| attrs.contains_key(k);
    if has("SubnetId") && has("SubnetIds") {
        return Err(mutual_exclusive(ctx, "SubnetId", "SubnetIds", ""));
    }
    if has("AvailabilityZone") && has("AvailabilityZones") {
        return Err(mutual_exclusive(
            ctx,
            "AvailabilityZone",
            "AvailabilityZones",
            "",
        ));
    }
    if (has("SubnetId") || has("SubnetIds"))
        && (has("AvailabilityZone") || has("AvailabilityZones"))
    {
        return Err(param_error(
            ctx,
            "You may not specify both a SubnetId and an AvailabilityZone (placement) because ec2SubnetId implies a placement.",
        ));
    }
    let mut instances = cluster.get("Instances").map(map_of).unwrap_or_default();
    apply_params(attrs, "KeyName", &mut instances, "Ec2KeyName");
    apply_params(attrs, "SubnetId", &mut instances, "Ec2SubnetId");
    apply_params(attrs, "SubnetIds", &mut instances, "Ec2SubnetIds");
    for key in ["AvailabilityZone", "AvailabilityZones"] {
        if has(key) {
            let mut placement = Map::new();
            apply_params(attrs, key, &mut placement, key);
            instances.insert("Placement".to_string(), Value::Map(placement));
        }
    }
    apply_params(attrs, "InstanceProfile", cluster, "JobFlowRole");
    for key in [
        "EmrManagedMasterSecurityGroup",
        "EmrManagedSlaveSecurityGroup",
        "ServiceAccessSecurityGroup",
        "AdditionalMasterSecurityGroups",
        "AdditionalSlaveSecurityGroups",
    ] {
        apply_params(attrs, key, &mut instances, key);
    }
    apply(cluster, "Instances", Some(Value::Map(instances)));
    Ok(())
}

/// `CreateCluster._build_bootstrap_actions`.
fn build_bootstrap_actions(
    ctx: &Context,
    cluster: &mut Map,
    bas: &[Value],
) -> Result<(), CliError> {
    let mut existing = list_of(cluster.get("BootstrapActions"));
    if existing.len() + bas.len() > 16 {
        return Err(param_error(
            ctx,
            "maximum number of bootstrap actions for a cluster exceeded.",
        ));
    }
    for ba in bas {
        let ba = map_of(ba);
        let mut config = Map::new();
        config.insert(
            "Name".to_string(),
            match ba.get("Name") {
                Some(v) if !matches!(v, Value::Null) => v.clone(),
                _ => Value::str("Bootstrap action"),
            },
        );
        let mut script = Map::new();
        apply_params(&ba, "Path", &mut script, "Path");
        apply_params(&ba, "Args", &mut script, "Args");
        apply(
            &mut config,
            "ScriptBootstrapAction",
            Some(Value::Map(script)),
        );
        existing.push(Value::Map(config));
    }
    if !existing.is_empty() {
        cluster.insert("BootstrapActions".to_string(), Value::List(existing));
    }
    Ok(())
}

/// `CreateCluster._validate_required_applications`.
fn validate_required_applications(ctx: &Context, args: &Args) -> Result<(), CliError> {
    let specified: Vec<String> = list_of(args.get("applications"))
        .iter()
        .filter_map(|a| a.get("Name").and_then(Value::as_str).map(str::to_lowercase))
        .collect();
    let mut missing: Vec<String> = Vec::new();
    for step in list_of(args.get("steps")) {
        if missing.len() == 3 {
            break;
        }
        if let Some(t) = step.get("Type").and_then(Value::as_str) {
            let lower = t.to_lowercase();
            if ["hive", "pig", "impala"].contains(&lower.as_str()) && !specified.contains(&lower) {
                let title = title_case(t);
                if !missing.contains(&title) {
                    missing.push(title);
                }
            }
        }
    }
    if args.get("restore_from_hbase_backup").is_some()
        && !specified.iter().any(|s| s == "hbase")
        && !missing.iter().any(|m| m == "Hbase")
    {
        missing.push("Hbase".to_string());
    }
    if !missing.is_empty() {
        return Err(param_error(
            ctx,
            &format!(
                "Some of the steps require the following applications to be installed: {}. Please install the applications using --applications.",
                missing.join(", ")
            ),
        ));
    }
    Ok(())
}

/// `str.title()`.
fn title_case(s: &str) -> String {
    let mut out = String::new();
    let mut prev_cased = false;
    for c in s.chars() {
        if c.is_alphabetic() {
            if prev_cased {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_cased = true;
        } else {
            out.push(c);
            prev_cased = false;
        }
    }
    out
}

/// `emrutils.validate_s3_logging_configuration`.
fn validate_s3_logging_configuration(
    ctx: &Context,
    monitoring: &Value,
    log_uri: Option<&str>,
) -> Result<(), CliError> {
    if !monitoring.is_truthy() {
        return Ok(());
    }
    let Some(s3) = monitoring
        .get("S3LoggingConfiguration")
        .filter(|v| v.is_truthy())
    else {
        return Ok(());
    };
    let Some(Value::Map(policy)) = s3.get("LogTypeUploadPolicy") else {
        return Ok(());
    };
    if policy.is_empty() {
        return Ok(());
    }
    for (log_type, p) in policy {
        if !["system-logs", "application-logs", "persistent-ui-logs"].contains(&log_type.as_str()) {
            return Err(param_error(
                ctx,
                "Invalid log type specified for the current S3LoggingConfiguration. Supported log types: system-logs, application-logs, persistent-ui-logs",
            ));
        }
        let p = p.as_str().unwrap_or_default();
        if !["emr-managed", "on-customer-s3only", "disabled"].contains(&p) {
            return Err(param_error(
                ctx,
                "Invalid policy specified for the current S3LoggingConfiguration. Supported policies: emr-managed, on-customer-s3only, disabled",
            ));
        }
        if log_type == "persistent-ui-logs" && p == "on-customer-s3only" {
            return Err(param_error(
                ctx,
                "Invalid policy for log type 'persistent-ui-logs'. Supported values for persistent-ui-logs: emr-managed, disabled",
            ));
        }
    }
    let requires = |k: &str| policy.get(k).and_then(Value::as_str) != Some("disabled");
    let required = requires("system-logs") || requires("application-logs");
    let provided = log_uri.is_some_and(|l| !l.trim().is_empty());
    if required && !provided {
        return Err(param_error(
            ctx,
            "A valid S3 location (LogUri) is required for the current S3LoggingConfiguration. Please specify an S3 bucket and try again.",
        ));
    }
    if !required && provided {
        return Err(param_error(
            ctx,
            "LogUri must not be specified when system-logs and application-logs policies are both disabled in S3LoggingConfiguration",
        ));
    }
    Ok(())
}

fn upper(v: Option<&Value>) -> String {
    v.map(crate::pyfmt::py_str)
        .unwrap_or_default()
        .to_uppercase()
}

/// `emrfsutils._verify_emrfs_args`.
fn verify_emrfs_args(ctx: &Context, a: &Map) -> Result<(), CliError> {
    let has = |k: &str| a.contains_key(k);
    if has("Encryption")
        && !["CLIENTSIDE", "SERVERSIDE"].contains(&upper(a.get("Encryption")).as_str())
    {
        return Err(param_error(
            ctx,
            &format!(
                "The encryption type \"{}\" is invalid. You must specify either ServerSide or ClientSide",
                crate::pyfmt::py_str(&a["Encryption"])
            ),
        ));
    }
    if has("SSE") && has("Encryption") {
        return Err(param_error(
            ctx,
            &format!(
                "Both SSE={} and Encryption={} are configured for --emrfs. You must specify only one of the two.",
                crate::pyfmt::py_str(&a["SSE"]),
                crate::pyfmt::py_str(&a["Encryption"])
            ),
        ));
    }
    if has("Encryption") && upper(a.get("Encryption")) == "CLIENTSIDE" {
        if !has("ProviderType") {
            return Err(util::missing_parameters(
                ctx,
                "--emrfs Encryption=ClientSide",
                "ProviderType",
            ));
        }
        let provider = upper(a.get("ProviderType"));
        match provider.as_str() {
            "KMS" => verify_required(
                ctx,
                a,
                &["KMSKeyId"],
                "--emrfs Encryption=ClientSide,ProviderType=KMS",
            )?,
            "CUSTOM" => verify_required(
                ctx,
                a,
                &["CustomProviderLocation", "CustomProviderClass"],
                "--emrfs Encryption=ClientSide,ProviderType=Custom",
            )?,
            _ => {
                return Err(param_error(
                    ctx,
                    &format!(
                        "The client side encryption type \"{}\" is not supported. You must specify either KMS or Custom",
                        crate::pyfmt::py_str(&a["ProviderType"])
                    ),
                ));
            }
        }
    }
    if !has("Consistent") {
        verify_child(
            ctx,
            a,
            &["RetryCount", "RetryPeriod"],
            "--emrfs Consistent=true/false",
        )?;
    }
    if !need_cse(a, "KMS") {
        verify_child(
            ctx,
            a,
            &["KMSKeyId"],
            "--emrfs Encryption=ClientSide,ProviderType=KMS",
        )?;
    }
    if !need_cse(a, "CUSTOM") {
        verify_child(
            ctx,
            a,
            &["CustomProviderLocation", "CustomProviderClass"],
            "--emrfs Encryption=ClientSide,ProviderType=Custom",
        )?;
    }
    Ok(())
}

fn verify_required(
    ctx: &Context,
    a: &Map,
    required: &[&str],
    object: &str,
) -> Result<(), CliError> {
    let mut missing: Vec<String> = required
        .iter()
        .filter(|k| !a.contains_key(**k))
        .map(|k| k.to_string())
        .collect();
    if !missing.is_empty() {
        missing.sort();
        return Err(util::missing_parameters(ctx, object, &util::join(&missing)));
    }
    Ok(())
}

fn verify_child(ctx: &Context, a: &Map, children: &[&str], parent: &str) -> Result<(), CliError> {
    let mut invalid: Vec<String> = children
        .iter()
        .filter(|k| a.contains_key(**k))
        .map(|k| k.to_string())
        .collect();
    if !invalid.is_empty() {
        invalid.sort();
        return Err(param_error(
            ctx,
            &format!(
                "{parent} is not specified. Thus,  following parameters are invalid: {}",
                util::join(&invalid)
            ),
        ));
    }
    Ok(())
}

fn need_cse(a: &Map, kind: &str) -> bool {
    a.contains_key("Encryption")
        && upper(a.get("Encryption")) == "CLIENTSIDE"
        && a.contains_key("ProviderType")
        && upper(a.get("ProviderType")) == kind
}

/// `str(value).lower()` of a parsed shorthand value.
fn lower_str(v: &Value) -> String {
    crate::pyfmt::py_str(v).to_lowercase()
}

/// `emrfsutils._build_emrfs_properties`.
fn emrfs_properties(a: &Map) -> Vec<(String, String)> {
    let mut props: Vec<(String, String)> = Vec::new();
    let mut set = |k: &str, v: String| {
        if let Some(e) = props.iter_mut().find(|(key, _)| key == k) {
            e.1 = v;
        } else {
            props.push((k.to_string(), v));
        }
    };
    if let Some(c) = a.get("Consistent") {
        set("fs.s3.consistent", lower_str(c));
        if let Some(v) = a.get("RetryCount") {
            set("fs.s3.consistent.retryCount", crate::pyfmt::py_str(v));
        }
        if let Some(v) = a.get("RetryPeriod") {
            set(
                "fs.s3.consistent.retryPeriodSeconds",
                crate::pyfmt::py_str(v),
            );
        }
    }
    if a.contains_key("SSE")
        || (a.contains_key("Encryption") && upper(a.get("Encryption")) == "SERVERSIDE")
    {
        let v = a.get("SSE").map(lower_str).unwrap_or("true".to_string());
        set("fs.s3.enableServerSideEncryption", v);
    }
    if need_cse(a, "KMS") {
        set("fs.s3.cse.enabled", "true".to_string());
        set(
            "fs.s3.cse.encryptionMaterialsProvider",
            "com.amazon.ws.emr.hadoop.fs.cse.KMSEncryptionMaterialsProvider".to_string(),
        );
        set(
            "fs.s3.cse.kms.keyId",
            a.get("KMSKeyId")
                .map(crate::pyfmt::py_str)
                .unwrap_or_default(),
        );
    }
    if need_cse(a, "CUSTOM") {
        set("fs.s3.cse.enabled", "true".to_string());
        set(
            "fs.s3.cse.encryptionMaterialsProvider",
            a.get("CustomProviderClass")
                .map(crate::pyfmt::py_str)
                .unwrap_or_default(),
        );
    }
    if let Some(Value::List(extra)) = a.get("Args") {
        for arg in extra {
            let (k, v) = util::split_to_key_value(&crate::pyfmt::py_str(arg));
            set(&k, v);
        }
    }
    props
}

/// `emrfsutils.build_emrfs_confiuration`.
fn emrfs_configuration(ctx: &Context, a: &Map) -> Result<Value, CliError> {
    verify_emrfs_args(ctx, a)?;
    let mut props = emrfs_properties(a);
    if need_cse(a, "CUSTOM") {
        props.push((
            "fs.s3.cse.encryptionMaterialsProvider.uri".to_string(),
            a.get("CustomProviderLocation")
                .map(crate::pyfmt::py_str)
                .unwrap_or_default(),
        ));
    }
    let mut m = Map::new();
    m.insert("Classification".to_string(), Value::str("emrfs-site"));
    m.insert(
        "Properties".to_string(),
        Value::Map(props.into_iter().map(|(k, v)| (k, Value::str(v))).collect()),
    );
    Ok(Value::Map(m))
}

/// `emrfsutils.build_bootstrap_action_configs`.
fn emrfs_bootstrap_actions(
    ctx: &Context,
    region: Option<&str>,
    a: &Map,
) -> Result<Vec<Value>, CliError> {
    verify_emrfs_args(ctx, a)?;
    let mut out = Vec::new();
    if need_cse(a, "CUSTOM") {
        out.push(util::build_bootstrap_action(
            "file:/usr/share/aws/emr/scripts/s3get",
            "S3 get",
            Some(vec![
                "-s".to_string(),
                a.get("CustomProviderLocation")
                    .map(crate::pyfmt::py_str)
                    .unwrap_or_default(),
                "-d".to_string(),
                "/usr/share/aws/emr/auxlib".to_string(),
                "-f".to_string(),
            ]),
        ));
    }
    let mut ba_args = Vec::new();
    for (k, v) in emrfs_properties(a) {
        ba_args.push("-e".to_string());
        if v.is_empty() {
            ba_args.push(k);
        } else {
            ba_args.push(format!("{k}={v}"));
        }
    }
    out.push(util::build_bootstrap_action(
        &util::s3_link("/bootstrap-actions/configure-hadoop", region),
        "Setup EMRFS",
        Some(ba_args),
    ));
    Ok(out)
}
