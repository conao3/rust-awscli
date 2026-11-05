//! Command tables (`CLIDriver._build_command_table`,
//! `ServiceCommand._create_command_table`, the `building-command-table`
//! customizations, `awscli/customizations/waiters.py`).

use crate::model::ServiceModel;

use super::arguments::xform_name;
use super::tables::{MAIN_COMMANDS, SERVICE_TABLE_DIFFS};

/// An entry of the main command table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MainCommand {
    /// A model-driven service: (CLI name, botocore service name).
    Service(&'static str, &'static str),
    /// A command that is not model driven (`s3`, `configure`, ...).
    Custom(&'static str),
}

/// The main command table, without `help`.
pub(crate) fn main_commands() -> impl Iterator<Item = MainCommand> {
    MAIN_COMMANDS.iter().map(|(name, service)| match service {
        Some(s) => MainCommand::Service(name, s),
        None => MainCommand::Custom(name),
    })
}

/// The choices of the `command` positional: the command table plus `help`
/// (`CLIDriver.create_parser`).
pub(crate) fn main_choices() -> Vec<String> {
    let mut v: Vec<String> = MAIN_COMMANDS.iter().map(|(n, _)| n.to_string()).collect();
    let _ = super::alias::extend_choices(&[], &mut v);
    v.push("help".to_string());
    v
}

pub(crate) fn find_main(name: &str) -> Option<MainCommand> {
    main_commands().find(|c| match c {
        MainCommand::Service(n, _) | MainCommand::Custom(n) => *n == name,
    })
}

/// An entry of a service command table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServiceEntry {
    /// A `ServiceOperation` for the botocore operation.
    Operation(String),
    /// The `wait` command (`waiters.WaitCommand`).
    Wait,
    /// A command added by a service customization.
    Custom,
}

/// `ServiceCommand._create_command_table` after the
/// `building-command-table.<service>` handlers: (name, entry) in order.
pub(crate) fn service_commands(
    cli_name: &str,
    model: &ServiceModel,
) -> Vec<(String, ServiceEntry)> {
    let mut table: Vec<(String, ServiceEntry)> = model
        .cli_operation_names()
        .into_iter()
        .map(|(op, cli)| (cli.to_string(), ServiceEntry::Operation(op.to_string())))
        .collect();
    if model.waiters().is_some_and(|w| !w.is_empty()) {
        table.push(("wait".to_string(), ServiceEntry::Wait));
    }
    if let Some((_, removed, renamed, custom)) =
        SERVICE_TABLE_DIFFS.iter().find(|(n, ..)| *n == cli_name)
    {
        table.retain(|(n, _)| !removed.contains(&n.as_str()));
        for (name, op) in *renamed {
            table.push((name.to_string(), ServiceEntry::Operation(op.to_string())));
        }
        for name in *custom {
            table.push((name.to_string(), ServiceEntry::Custom));
        }
    }
    table
}

/// The waiter subcommands of `wait` (`WaiterStateCommandBuilder`):
/// (CLI name, waiter name).
pub(crate) fn waiter_commands(model: &ServiceModel) -> Vec<(String, String)> {
    model
        .waiters()
        .map(|w| {
            let mut names: Vec<&str> = w.keys().collect();
            names.sort();
            names
                .into_iter()
                .map(|n| (xform_name(n, '-'), n.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Operations whose argument table or parameters a service-specific
/// customization changes (`building-argument-table.<service>.<operation>`,
/// `operation-args-parsed`, `process-cli-arg`, `before-parameter-build`
/// handlers of `awscli/handlers_registry.py` other than the ones ported
/// here). They are reported as not implemented rather than parsed
/// differently from the reference.
static CUSTOMIZED_OPERATIONS: &[(&str, &str)] = &[
    ("cloudfront", "create-distribution"),
    ("cloudfront", "create-invalidation"),
    ("cloudfront", "update-distribution"),
    ("cloudsearch", "define-expression"),
    ("cloudsearch", "define-index-field"),
    ("cloudwatch", "put-metric-data"),
    ("configservice", "put-configuration-recorder"),
    ("datapipeline", "activate-pipeline"),
    ("datapipeline", "put-pipeline-definition"),
    ("deploy", "create-deployment"),
    ("deploy", "get-application-revision"),
    ("deploy", "register-application-revision"),
    ("ec2", "authorize-security-group-egress"),
    ("ec2", "authorize-security-group-ingress"),
    ("ec2", "bundle-instance"),
    ("ec2", "get-password-data"),
    ("ec2", "revoke-security-group-egress"),
    ("ec2", "revoke-security-group-ingress"),
    ("ecs", "create-express-gateway-service"),
    ("ecs", "delete-express-gateway-service"),
    ("ecs", "update-express-gateway-service"),
    ("emr", "add-tags"),
    ("emr", "list-clusters"),
    ("iam", "create-virtual-mfa-device"),
    ("iot", "create-certificate-from-csr"),
    ("iot", "create-keys-and-certificate"),
    ("kinesis", "list-streams"),
    ("lambda", "create-function"),
    ("lambda", "publish-layer-version"),
    ("lambda", "update-function-code"),
    ("quicksight", "start-asset-bundle-import-job"),
    ("rds", "add-option-to-option-group"),
    ("rds", "remove-option-from-option-group"),
    ("rekognition", "compare-faces"),
    ("rekognition", "detect-custom-labels"),
    ("rekognition", "detect-faces"),
    ("rekognition", "detect-labels"),
    ("rekognition", "detect-moderation-labels"),
    ("rekognition", "detect-protective-equipment"),
    ("rekognition", "detect-text"),
    ("rekognition", "index-faces"),
    ("rekognition", "recognize-celebrities"),
    ("rekognition", "search-faces-by-image"),
    ("rekognition", "search-users-by-image"),
    ("s3api", "select-object-content"),
    ("ses", "send-email"),
    ("translate", "import-terminology"),
    ("translate", "translate-document"),
];

pub(crate) fn is_customized(service: &str, operation: &str) -> bool {
    CUSTOMIZED_OPERATIONS
        .iter()
        .any(|(s, o)| *s == service && *o == operation)
}
