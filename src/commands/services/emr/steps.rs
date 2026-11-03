//! `awscli/customizations/emr/steputils.py` and `applicationutils.py`.

use super::util::{
    self, COMMAND_RUNNER, DEFAULT_FAILURE_ACTION, LATEST, Step, TERMINATE_CLUSTER, s3_link,
};
use crate::driver::Context;
use crate::error::CliError;
use crate::value::{Map, Value};

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::List(l)) => l.iter().map(crate::pyfmt::py_str).collect(),
        Some(Value::Str(s)) => s.chars().map(|c| c.to_string()).collect(),
        _ => Vec::new(),
    }
}

fn get_str<'a>(step: &'a Map, key: &str) -> Option<&'a str> {
    step.get(key).and_then(Value::as_str)
}

fn runner_jar(release_label: Option<&str>, region: Option<&str>) -> String {
    match release_label {
        Some(l) if !l.is_empty() => COMMAND_RUNNER.to_string(),
        _ => util::script_runner(region),
    }
}

/// `steputils.build_step_config_list`.
pub(crate) fn build_step_config_list(
    ctx: &Context,
    steps: &[Value],
    region: Option<&str>,
    release_label: Option<&str>,
) -> Result<Vec<Value>, CliError> {
    let release = release_label.filter(|l| !l.is_empty());
    let mut out = Vec::new();
    for step in steps {
        let empty = Map::new();
        let step = step.as_map().unwrap_or(&empty);
        let step_type = get_str(step, "Type").unwrap_or("custom_jar").to_lowercase();
        let name = |default: &'static str| -> String {
            get_str(step, "Name")
                .map(str::to_string)
                .unwrap_or(default.to_string())
        };
        let action = get_str(step, "ActionOnFailure")
            .unwrap_or(DEFAULT_FAILURE_ACTION)
            .to_string();
        let args = step.get("Args");
        let log_uri = step.get("LogUri").cloned();
        let key_arn = step.get("EncryptionKeyArn").cloned();
        let config = match step_type.as_str() {
            "custom_jar" => {
                let name = name("Custom JAR");
                util::check_required_field(ctx, "CustomJARStepConfig", "Jar", step.get("Jar"))?;
                util::build_step(
                    ctx,
                    Step {
                        jar: step.get("Jar").cloned(),
                        name: &name,
                        action_on_failure: &action,
                        args: args.cloned(),
                        main_class: step.get("MainClass").cloned(),
                        properties: util::parse_key_value_string(step.get("Properties")),
                        log_uri,
                        encryption_key_arn: key_arn,
                    },
                )?
            }
            "streaming" => {
                let name = name("Streaming program");
                util::check_required_field(ctx, "StreamingStepConfig", "Args", args)?;
                util::check_empty_string_list(ctx, "Args", args)?;
                let mut list = Vec::new();
                let jar = if release.is_some() {
                    list.push("hadoop-streaming".to_string());
                    COMMAND_RUNNER.to_string()
                } else {
                    "/home/hadoop/contrib/streaming/hadoop-streaming.jar".to_string()
                };
                list.extend(strings(args));
                built(ctx, &jar, &name, &action, list, log_uri, key_arn)?
            }
            "hive" | "pig" => {
                let (config_name, default, script, cmd, run, versions) = if step_type == "hive" {
                    (
                        "HiveStepConfig",
                        "Hive program",
                        "/libs/hive/hive-script",
                        "hive-script",
                        "--run-hive-script",
                        "--hive-versions",
                    )
                } else {
                    (
                        "PigStepConfig",
                        "Pig program",
                        "/libs/pig/pig-script",
                        "pig-script",
                        "--run-pig-script",
                        "--pig-versions",
                    )
                };
                util::check_required_field(ctx, config_name, "Args", args)?;
                util::check_empty_string_list(ctx, "Args", args)?;
                let name = name(default);
                let mut list = Vec::new();
                if release.is_some() {
                    list.push(cmd.to_string());
                } else {
                    list.push(s3_link(script, region));
                }
                list.push(run.to_string());
                if release.is_none() {
                    list.push(versions.to_string());
                    list.push(LATEST.to_string());
                }
                list.push("--args".to_string());
                list.extend(strings(args));
                built(
                    ctx,
                    &runner_jar(release, region),
                    &name,
                    &action,
                    list,
                    log_uri,
                    key_arn,
                )?
            }
            "impala" => {
                if release.is_some() {
                    return Err(unknown_step_type(ctx, "impala"));
                }
                let name = name("Impala program");
                let mut list = vec![
                    s3_link("/libs/impala/setup-impala", region),
                    "--run-impala-script".to_string(),
                ];
                util::check_required_field(ctx, "ImpalaStepConfig", "Args", args)?;
                list.extend(strings(args));
                built(
                    ctx,
                    &util::script_runner(region),
                    &name,
                    &action,
                    list,
                    log_uri,
                    key_arn,
                )?
            }
            "spark" => {
                let name = name("Spark application");
                util::check_required_field(ctx, "SparkStepConfig", "Args", args)?;
                let mut list = vec![if release.is_some() {
                    "spark-submit".to_string()
                } else {
                    "/home/hadoop/spark/bin/spark-submit".to_string()
                }];
                list.extend(strings(args));
                built(
                    ctx,
                    &runner_jar(release, region),
                    &name,
                    &action,
                    list,
                    log_uri,
                    key_arn,
                )?
            }
            other => return Err(unknown_step_type(ctx, other)),
        };
        out.push(config);
    }
    Ok(out)
}

fn unknown_step_type(ctx: &Context, step_type: &str) -> CliError {
    util::param_error(ctx, &format!("The step type {step_type} is not supported."))
}

fn built(
    ctx: &Context,
    jar: &str,
    name: &str,
    action: &str,
    args: Vec<String>,
    log_uri: Option<Value>,
    key_arn: Option<Value>,
) -> Result<Value, CliError> {
    let mut s = Step::new(jar, name, action, args);
    s.log_uri = log_uri;
    s.encryption_key_arn = key_arn;
    util::build_step(ctx, s)
}

/// (applications, bootstrap actions, steps).
pub(crate) type Built = (Vec<Value>, Vec<Value>, Vec<Value>);

/// `applicationutils.build_applications`: (applications, bootstrap
/// actions, steps).
pub(crate) fn build_applications(
    ctx: &Context,
    region: Option<&str>,
    applications: &[Value],
    ami_version: Option<&str>,
) -> Result<Built, CliError> {
    let mut apps = Vec::new();
    let mut bas = Vec::new();
    let mut steps = Vec::new();
    for app in applications {
        let empty = Map::new();
        let app = app.as_map().unwrap_or(&empty);
        let name = get_str(app, "Name").unwrap_or_default().to_string();
        let args = app.get("Args");
        match name.to_lowercase().as_str() {
            "hive" => {
                steps.push(install_step(
                    ctx,
                    "Install Hive",
                    TERMINATE_CLUSTER,
                    vec![
                        s3_link("/libs/hive/hive-script", region),
                        "--install-hive".to_string(),
                        "--base-path".to_string(),
                        s3_link("/libs/hive", region),
                        "--hive-versions".to_string(),
                        LATEST.to_string(),
                    ],
                    region,
                )?);
                if args.is_some_and(|a| !matches!(a, Value::Null))
                    && let Some(path) = strings(args)
                        .into_iter()
                        .find(|a| a.contains("--hive-site"))
                {
                    steps.push(install_step(
                        ctx,
                        "Install Hive Site Configuration",
                        util::CANCEL_AND_WAIT,
                        vec![
                            s3_link("/libs/hive/hive-script", region),
                            "--base-path".to_string(),
                            s3_link("/libs/hive", None),
                            "--install-hive-site".to_string(),
                            path,
                            "--hive-versions".to_string(),
                            LATEST.to_string(),
                        ],
                        region,
                    )?);
                }
            }
            "pig" => steps.push(install_step(
                ctx,
                "Install Pig",
                TERMINATE_CLUSTER,
                vec![
                    s3_link("/libs/pig/pig-script", region),
                    "--install-pig".to_string(),
                    "--base-path".to_string(),
                    s3_link("/libs/pig", region),
                    "--pig-versions".to_string(),
                    LATEST.to_string(),
                ],
                region,
            )?),
            "ganglia" => bas.push(util::build_bootstrap_action(
                &s3_link("/bootstrap-actions/install-ganglia", region),
                "Install Ganglia",
                None,
            )),
            "hbase" => {
                bas.push(util::build_bootstrap_action(
                    &s3_link("/bootstrap-actions/setup-hbase", region),
                    "Install HBase",
                    None,
                ));
                let ami = ami_version.unwrap_or_default();
                let jar = if ami >= "3.0" {
                    "/home/hadoop/lib/hbase.jar"
                } else if ami >= "2.1" {
                    "/home/hadoop/lib/hbase-0.92.0.jar"
                } else {
                    return Err(util::param_error(
                        ctx,
                        &format!("AMI version {ami} is not compatible with HBase."),
                    ));
                };
                steps.push(util::build_step(
                    ctx,
                    Step::new(
                        jar,
                        "Start HBase",
                        TERMINATE_CLUSTER,
                        vec![
                            "emr.hbase.backup.Main".to_string(),
                            "--start-master".to_string(),
                        ],
                    ),
                )?);
            }
            "impala" => {
                let mut list = vec![
                    "--base-path".to_string(),
                    s3_link("", region),
                    "--impala-version".to_string(),
                    LATEST.to_string(),
                ];
                if args.is_some_and(|a| !matches!(a, Value::Null)) {
                    list.push("--impala-conf".to_string());
                    list.push(strings(args).join(","));
                }
                bas.push(util::build_bootstrap_action(
                    &s3_link("/libs/impala/setup-impala", region),
                    "Install Impala",
                    Some(list),
                ));
            }
            _ => {
                let mut m = Map::new();
                m.insert("Name".to_string(), Value::str(name.to_lowercase()));
                m.insert(
                    "Args".to_string(),
                    match args {
                        None | Some(Value::Null) => Value::List(Vec::new()),
                        Some(a) => a.clone(),
                    },
                );
                apps.push(Value::Map(m));
            }
        }
    }
    Ok((apps, bas, steps))
}

fn install_step(
    ctx: &Context,
    name: &str,
    action: &str,
    args: Vec<String>,
    region: Option<&str>,
) -> Result<Value, CliError> {
    util::build_step(
        ctx,
        Step::new(
            &s3_link(util::SCRIPT_RUNNER_PATH, region),
            name,
            action,
            args,
        ),
    )
}
