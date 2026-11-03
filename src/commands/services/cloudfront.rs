//! `cloudfront` customizations (`awscli/customizations/cloudfront/`):
//! `create-invalidation --paths`, `create-distribution
//! --origin-domain-name / --default-root-object`, `update-distribution
//! --default-root-object`, and the `sign` / `sign-cookies` commands.

mod sign;

use indexmap::IndexMap;

use super::cloudwatch::{unique_string, validate_mutually_exclusive, value_str};
use super::exec::{self, Client};
use crate::cli::argparse::{Nargs, NsVal};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "create-invalidation" => {
            exec::run_operation(ctx, "cloudfront", name, args, &mut CreateInvalidation)
        }
        "create-distribution" => {
            exec::run_operation(ctx, "cloudfront", name, args, &mut CreateDistribution)
        }
        "update-distribution" => {
            let mut hooks = UpdateDistribution { ctx, error: None };
            let parsed = exec::parse_operation(ctx, "cloudfront", name, args, &mut hooks);
            if let Some(e) = hooks.error {
                return Err(e);
            }
            match parsed? {
                Some(inv) => exec::invoke(ctx, inv, &mut |_| Ok(exec::After::Display)),
                None => Ok(ExitCode::Success),
            }
        }
        "sign" => sign::sign(ctx, args),
        "sign-cookies" => sign::sign_cookies(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

fn set_not_required(table: &mut ArgTable<'_>, name: &str) {
    if let Some(a) = table.get_mut(name) {
        a.required = false;
    }
}

fn scalar_arg<'m>(table: &mut ArgTable<'m>, name: &str) {
    table.insert(
        name.to_string(),
        Arg::custom_argument(name, CustomSpec::default(), false),
    );
}

struct CreateInvalidation;

impl OperationHooks for CreateInvalidation {
    /// `add_paths_argument`.
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        set_not_required(table, "invalidation-batch");
        table.insert(
            "paths".to_string(),
            Arg::custom_argument(
                "paths",
                CustomSpec {
                    nargs: Some(Nargs::OneOrMore),
                    ..CustomSpec::default()
                },
                false,
            ),
        );
    }

    fn args_parsed(&mut self, ns: &IndexMap<String, NsVal>, _: &mut bool) -> Result<(), Fail> {
        validate_mutually_exclusive(ns, &[&["invalidation_batch"], &["paths"]])
    }

    /// `PathsArgument.add_to_params`.
    fn add_to_params(
        &mut self,
        _name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        let items = match value {
            Value::List(l) => l,
            other => vec![other],
        };
        let mut paths = Map::new();
        paths.insert("Quantity".to_string(), Value::int(items.len() as i64));
        paths.insert("Items".to_string(), Value::List(items));
        let mut batch = Map::new();
        batch.insert(
            "CallerReference".to_string(),
            Value::str(unique_string("cli")),
        );
        batch.insert("Paths".to_string(), Value::Map(paths));
        params.insert("InvalidationBatch".to_string(), Value::Map(batch));
        Ok(())
    }
}

/// `ExclusiveArgument.distribution_config_template`.
fn distribution_config_template() -> Value {
    let v = serde_json::json!({
        "CallerReference": unique_string("cli"),
        "Origins": {"Quantity": 0, "Items": []},
        "DefaultCacheBehavior": {
            "TargetOriginId": "placeholder",
            "ForwardedValues": {
                "QueryString": false,
                "Cookies": {"Forward": "none"},
            },
            "TrustedSigners": {"Enabled": false, "Quantity": 0},
            "ViewerProtocolPolicy": "allow-all",
            "MinTTL": 0,
        },
        "Enabled": true,
        "Comment": "",
    });
    Value::from_json(&v)
}

fn distribution_config(params: &mut Map) -> &mut Map {
    let entry = params
        .entry("DistributionConfig".to_string())
        .or_insert_with(distribution_config_template);
    if !matches!(entry, Value::Map(_)) {
        *entry = Value::map();
    }
    entry.as_map_mut().expect("map")
}

struct CreateDistribution;

impl OperationHooks for CreateDistribution {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        set_not_required(table, "distribution-config");
        scalar_arg(table, "origin-domain-name");
        scalar_arg(table, "default-root-object");
    }

    fn args_parsed(&mut self, ns: &IndexMap<String, NsVal>, _: &mut bool) -> Result<(), Fail> {
        validate_mutually_exclusive(
            ns,
            &[
                &["default_root_object", "origin_domain_name"],
                &["distribution_config"],
            ],
        )
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        match name {
            "origin-domain-name" => {
                let domain = value_str(&value);
                let config = distribution_config(params);
                let origin_id = unique_string(&domain);
                let item = if domain.ends_with(".s3.amazonaws.com") {
                    serde_json::json!({
                        "Id": origin_id, "DomainName": domain, "OriginPath": "",
                        "S3OriginConfig": {"OriginAccessIdentity": ""}
                    })
                } else {
                    serde_json::json!({
                        "Id": origin_id, "DomainName": domain, "OriginPath": "",
                        "CustomOriginConfig": {
                            "HTTPPort": 80, "HTTPSPort": 443,
                            "OriginProtocolPolicy": "http-only"
                        }
                    })
                };
                config.insert(
                    "Origins".to_string(),
                    Value::from_json(&serde_json::json!({"Quantity": 1, "Items": [item]})),
                );
                match config.get_mut("DefaultCacheBehavior") {
                    Some(Value::Map(b)) => {
                        b.insert("TargetOriginId".to_string(), Value::Str(origin_id));
                    }
                    _ => return Err(Fail::General("'DefaultCacheBehavior'".to_string())),
                }
            }
            _ => {
                distribution_config(params).insert("DefaultRootObject".to_string(), value);
            }
        }
        Ok(())
    }
}

struct UpdateDistribution<'c> {
    ctx: &'c Context,
    /// A service error of the `get-distribution-config` call.
    error: Option<CliError>,
}

impl OperationHooks for UpdateDistribution<'_> {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        set_not_required(table, "distribution-config");
        scalar_arg(table, "default-root-object");
    }

    fn args_parsed(&mut self, ns: &IndexMap<String, NsVal>, _: &mut bool) -> Result<(), Fail> {
        validate_mutually_exclusive(ns, &[&["default_root_object"], &["distribution_config"]])
    }

    /// `UpdateDefaultRootObject.add_to_params`.
    fn add_to_params(
        &mut self,
        _name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        let Some(id) = params.get("Id").cloned() else {
            return Err(Fail::General("'Id'".to_string()));
        };
        let mut request = Map::new();
        request.insert("Id".to_string(), id);
        let response = Client::new(self.ctx, "cloudfront")
            .and_then(|c| c.call("GetDistributionConfig", &Value::Map(request)));
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                return Err(Fail::General(String::new()));
            }
        };
        let etag = response.get("ETag").cloned().unwrap_or(Value::Null);
        params.insert("IfMatch".to_string(), etag);
        let mut config = response
            .get("DistributionConfig")
            .cloned()
            .unwrap_or_else(Value::map);
        if let Value::Map(m) = &mut config {
            m.insert("DefaultRootObject".to_string(), value);
        }
        params.insert("DistributionConfig".to_string(), config);
        Ok(())
    }
}
