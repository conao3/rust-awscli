//! Service customizations: commands a customization adds to a service
//! table (`building-command-table.<service>` handlers of
//! `awscli/handlers_registry.py`) and model operations whose arguments a
//! customization reshapes (`crate::cli` routes both here).

#[allow(dead_code)]
pub(crate) mod basic;
#[allow(dead_code)]
pub(crate) mod exec;

mod agent_toolkit;
mod cloudformation;
mod cloudfront;
mod cloudsearch;
mod cloudtrail;
mod cloudwatch;
mod codeartifact;
mod codecommit;
mod configservice;
mod datapipeline;
mod deploy;
mod dlm;
mod dsql;
mod dynamodb;
mod ec2;
mod ec2_instance_connect;
mod ecr;
mod ecs;
mod eks;
mod emr;
mod emr_containers;
mod events;
mod gamelift;
mod iam;
mod iot;
mod kinesis;
mod lambda;
mod lambda_web;
mod lightsail;
mod logs;
pub(crate) mod presign;
mod quicksight;
mod rds;
mod rekognition;
mod s3api;
mod servicecatalog;
mod ses;
mod ssm;
mod translate;
pub(crate) mod wizard;

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

/// Whether `service` has custom commands or customized operations here.
pub(crate) fn handles(service: &str) -> bool {
    module(service).is_some()
}

type Run = fn(&Context, &str, &[String]) -> Result<ExitCode, CliError>;

fn module(service: &str) -> Option<Run> {
    Some(match service {
        "agent-toolkit" => agent_toolkit::run,
        "cloudformation" => cloudformation::run,
        "cloudfront" => cloudfront::run,
        "cloudsearch" => cloudsearch::run,
        "cloudtrail" => cloudtrail::run,
        "cloudwatch" => cloudwatch::run,
        "codeartifact" => codeartifact::run,
        "codecommit" => codecommit::run,
        "configservice" => configservice::run,
        "datapipeline" => datapipeline::run,
        "deploy" => deploy::run,
        "dlm" => dlm::run,
        "dsql" => dsql::run,
        "dynamodb" => dynamodb::run,
        "ec2" => ec2::run,
        "ec2-instance-connect" => ec2_instance_connect::run,
        "ecr" => ecr::run,
        "ecr-public" => ecr::run_public,
        "ecs" => ecs::run,
        "eks" => eks::run,
        "emr" => emr::run,
        "emr-containers" => emr_containers::run,
        "events" => events::run,
        "gamelift" => gamelift::run,
        "iam" => iam::run,
        "iot" => iot::run,
        "kinesis" => kinesis::run,
        "lambda" => lambda::run,
        "lambda-web" => lambda_web::run,
        "lightsail" => lightsail::run,
        "logs" => logs::run,
        "quicksight" => quicksight::run,
        "rds" => rds::run,
        "rekognition" => rekognition::run,
        "s3api" => s3api::run,
        "servicecatalog" => servicecatalog::run,
        "ses" => ses::run,
        "ssm" => ssm::run,
        "translate" => translate::run,
        _ => return None,
    })
}

/// Run the command `service name` with `args`.
pub(crate) fn run(
    ctx: &Context,
    service: &str,
    name: &str,
    args: &[String],
) -> Result<ExitCode, CliError> {
    match module(service) {
        Some(run) => run(ctx, name, args),
        None => Err(crate::commands::not_implemented()),
    }
}
