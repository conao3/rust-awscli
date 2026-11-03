//! `--s3-location` / `--github-location` of `create-deployment`,
//! `get-application-revision`, and `register-application-revision`
//! (`codedeploy/locationargs.py`).

use serde_json::json;

use crate::cli::argprocess::{ArgContext, unpack_argument};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::commands::services::basic::transform_schema;
use crate::commands::services::exec;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::{Operation, ServiceModel};
use crate::value::{Map, Value};

pub(super) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    let mut hooks = Location {
        s3: ServiceModel::from_shapes(
            "custom",
            transform_schema(&json!({
                "type": "object",
                "properties": {
                    "bucket": {"type": "string", "required": true},
                    "key": {"type": "string", "required": true},
                    "bundleType": {"type": "string", "enum": ["tar", "tgz", "zip"], "required": true},
                    "eTag": {"type": "string", "required": false},
                    "version": {"type": "string", "required": false}
                }
            })),
        ),
        github: ServiceModel::from_shapes(
            "custom",
            transform_schema(&json!({
                "type": "object",
                "properties": {
                    "repository": {"type": "string", "required": true},
                    "commitId": {"type": "string", "required": true}
                }
            })),
        ),
    };
    exec::run_operation(ctx, "deploy", name, args, &mut hooks)
}

struct Location {
    s3: ServiceModel,
    github: ServiceModel,
}

impl OperationHooks for Location {
    /// `modify_revision_arguments`.
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        for name in ["s3-location", "github-location"] {
            table.insert(
                name.to_string(),
                Arg::custom_argument(name, CustomSpec::default(), false),
            );
        }
        if let Some(a) = table.get_mut("revision") {
            a.required = false;
        }
    }

    /// `LocationArgument.add_to_params`.
    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        let model = if name == "s3-location" {
            &self.s3
        } else {
            &self.github
        };
        let shape = model.shape("InputShape").expect("InputShape");
        let cli_name = format!("--{name}");
        let parsed = unpack_argument(
            &ArgContext {
                name,
                cli_name: &cli_name,
                shape: &shape,
                service: "codedeploy",
                operation: name,
            },
            &value,
        )?;
        let Value::Map(d) = parsed else {
            return Err(Fail::General(
                "'str' object has no attribute 'get'".to_string(),
            ));
        };
        let valid = |k: &str| d.get(k).is_some_and(Value::is_truthy);
        let revision = if name == "s3-location" {
            if !["bucket", "key", "bundleType"].iter().all(|k| valid(k)) {
                return Err(Fail::ParamValidation(
                    "--s3-location must specify bucket, key and bundleType.".to_string(),
                ));
            }
            let mut loc = Map::new();
            for k in ["bucket", "key", "bundleType"] {
                loc.insert(k.to_string(), d[k].clone());
            }
            for k in ["eTag", "version"] {
                if let Some(v) = d.get(k) {
                    loc.insert(k.to_string(), v.clone());
                }
            }
            let mut r = Map::new();
            r.insert("revisionType".to_string(), Value::str("S3"));
            r.insert("s3Location".to_string(), Value::Map(loc));
            r
        } else {
            if !["repository", "commitId"].iter().all(|k| valid(k)) {
                return Err(Fail::ParamValidation(
                    "--github-location must specify repository and commitId.".to_string(),
                ));
            }
            let mut loc = Map::new();
            for k in ["repository", "commitId"] {
                loc.insert(k.to_string(), d[k].clone());
            }
            let mut r = Map::new();
            r.insert("revisionType".to_string(), Value::str("GitHub"));
            r.insert("gitHubLocation".to_string(), Value::Map(loc));
            r
        };
        params.insert("revision".to_string(), Value::Map(revision));
        Ok(())
    }
}
