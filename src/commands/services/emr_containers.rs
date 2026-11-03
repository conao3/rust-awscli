//! The `emr-containers` commands (`awscli/customizations/emrcontainers/`):
//! `update-role-trust-policy`, `create-role-associations`,
//! `delete-role-associations`.

use num_bigint::BigUint;
use num_traits::Zero;

use super::basic::{self, Args};
use super::emr::util::{self, general_error};
use super::exec::Client;
use crate::driver::{CallResult, Context};
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    if !matches!(
        name,
        "update-role-trust-policy" | "create-role-associations" | "delete-role-associations"
    ) {
        return Err(crate::commands::not_implemented());
    }
    let defs = util::arg_defs(&format!("emr-containers {name}"));
    let Some(parsed) = basic::parse(ctx, &["emr-containers", name], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    match name {
        "update-role-trust-policy" => update_role_trust_policy(ctx, &parsed)?,
        "create-role-associations" => role_associations(ctx, &parsed, true)?,
        _ => role_associations(ctx, &parsed, false)?,
    }
    Ok(ExitCode::Success)
}

/// `Base36.encode`.
fn base36(s: &str) -> String {
    let mut n = BigUint::zero();
    for c in s.chars() {
        n = n * 256u32 + u32::from(c);
    }
    if n.is_zero() {
        return "0".to_string();
    }
    n.to_str_radix(36)
}

/// The `eks` client the commands create (no `--endpoint-url`).
fn eks_context(ctx: &Context) -> Result<Context, CliError> {
    util::context_with_endpoint(ctx, None)
}

struct Eks<'c> {
    client: Client<'c>,
    cluster: Option<Value>,
}

impl<'c> Eks<'c> {
    fn describe(&mut self, name: &str) -> Result<&Value, CliError> {
        if self.cluster.is_none() {
            let mut p = Map::new();
            p.insert("name".to_string(), Value::str(name));
            self.cluster = Some(self.client.call("DescribeCluster", &Value::Map(p))?);
        }
        Ok(self.cluster.as_ref().expect("cluster"))
    }

    /// `EKS.get_account_id`.
    fn account_id(&mut self, name: &str) -> Result<String, CliError> {
        let ctx = self.client.ctx;
        let arn = self
            .describe(name)?
            .get("cluster")
            .and_then(|c| c.get("arn"))
            .map(crate::pyfmt::py_str)
            .unwrap_or_default();
        arn.split(':')
            .nth(4)
            .map(str::to_string)
            .ok_or_else(|| general_error(ctx, "list index out of range"))
    }

    /// `EKS.get_oidc_issuer_id`.
    fn oidc_issuer_id(&mut self, name: &str) -> Result<String, CliError> {
        let ctx = self.client.ctx;
        let issuer = self
            .describe(name)?
            .get("cluster")
            .and_then(|c| c.get("identity"))
            .and_then(|c| c.get("oidc"))
            .and_then(|c| c.get("issuer"))
            .map(crate::pyfmt::py_str)
            .unwrap_or_default();
        issuer
            .split("https://")
            .nth(1)
            .map(str::to_string)
            .ok_or_else(|| general_error(ctx, "list index out of range"))
    }
}

fn region(ctx: &Context) -> String {
    util::region(ctx).unwrap_or_default()
}

/// `check_if_dict_matches`.
fn dict_matches(expected: &Map, actual: &Map) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    for (key, val) in expected {
        match val {
            Value::Map(e) => {
                let empty = Map::new();
                let a = actual.get(key).and_then(Value::as_map).unwrap_or(&empty);
                if !dict_matches(e, a) {
                    return false;
                }
            }
            v => match actual.get(key) {
                Some(Value::Str(a)) if *a == crate::pyfmt::py_str(v) => {}
                _ => return false,
            },
        }
    }
    true
}

/// `UpdateRoleTrustPolicyCommand._update_role_trust_policy`.
fn update_role_trust_policy(ctx: &Context, parsed: &Args) -> Result<(), CliError> {
    let cluster_name = parsed.str("cluster_name").unwrap_or_default();
    let namespace = parsed.str("namespace").unwrap_or_default();
    let role_name = parsed.str("role_name").unwrap_or_default();
    let region = region(ctx);
    let eks_ctx = eks_context(ctx)?;
    let mut eks = Eks {
        client: Client::new(&eks_ctx, "eks")?,
        cluster: None,
    };
    let account_id = eks.account_id(&cluster_name)?;
    let oidc = eks.oidc_issuer_id(&cluster_name)?;
    let encoded = base36(&role_name);
    let partition = util::policy_arn_suffix(&region);
    let mut federated = Map::new();
    federated.insert(
        "Federated".to_string(),
        Value::str(format!(
            "arn:{partition}:iam::{account_id}:oidc-provider/{oidc}"
        )),
    );
    let mut like = Map::new();
    like.insert(
        format!("{oidc}:sub"),
        Value::str(format!(
            "system:serviceaccount:{namespace}:emr-containers-sa-*-*-{account_id}-{encoded}"
        )),
    );
    let mut condition = Map::new();
    condition.insert("StringLike".to_string(), Value::Map(like));
    let mut statement = Map::new();
    statement.insert("Effect".to_string(), Value::str("Allow"));
    statement.insert("Principal".to_string(), Value::Map(federated));
    statement.insert(
        "Action".to_string(),
        Value::str("sts:AssumeRoleWithWebIdentity"),
    );
    statement.insert("Condition".to_string(), Value::Map(condition));

    let iam_ctx = util::context_with_endpoint(ctx, parsed.str("iam_endpoint"))?;
    let iam = Client::new(&iam_ctx, "iam")?;
    let mut p = Map::new();
    p.insert("RoleName".to_string(), Value::str(role_name.clone()));
    let mut role = iam.call("GetRole", &Value::Map(p))?;
    util::decode_policies(&iam, "GetRole", &mut role);
    let Some(role) = role.get("Role").filter(|r| !matches!(r, Value::Null)) else {
        return Err(general_error(
            ctx,
            "'NoneType' object has no attribute 'get'",
        ));
    };
    let mut document = match role.get("AssumeRolePolicyDocument") {
        Some(Value::Map(m)) => m.clone(),
        Some(Value::Str(_)) => {
            return Err(general_error(ctx, "'str' object has no attribute 'get'"));
        }
        _ => {
            return Err(general_error(
                ctx,
                "'NoneType' object has no attribute 'get'",
            ));
        }
    };
    let exists = match document.get("Statement") {
        Some(Value::List(l)) => l
            .iter()
            .any(|s| s.as_map().is_some_and(|s| dict_matches(&statement, s))),
        _ => false,
    };
    let result = if exists {
        format!("Trust policy statement already exists for role {role_name}. No changes were made!")
    } else {
        match document.get_mut("Statement") {
            Some(Value::List(l)) => l.push(Value::Map(statement)),
            _ => {
                document.insert(
                    "Statement".to_string(),
                    Value::List(vec![Value::Map(statement)]),
                );
            }
        }
        let document = Value::Map(document);
        if parsed.bool("dry_run") {
            util::json_dumps(&document, 2)
        } else {
            let mut p = Map::new();
            p.insert("RoleName".to_string(), Value::str(role_name.clone()));
            p.insert(
                "PolicyDocument".to_string(),
                Value::str(crate::pyfmt::json_dumps_default(&document)),
            );
            iam.call("UpdateAssumeRolePolicy", &Value::Map(p))?;
            format!("Successfully updated trust policy of role {role_name}")
        }
    };
    basic::print(&result);
    basic::print("\n");
    Ok(())
}

fn naming(framework: &str, component: &str, account_id: &str, encoded: &str) -> String {
    format!("emr-containers-sa-{framework}-{component}-{account_id}-{encoded}")
}

/// `_get_<type>_mapping`: (service account, namespace).
fn mapping(parsed: &Args, account_id: &str, encoded: &str) -> Vec<(String, String)> {
    let namespace = parsed.str("namespace").unwrap_or_default();
    let operator_namespace = parsed
        .str("operator_namespace")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| namespace.clone());
    let t = parsed
        .str("type")
        .filter(|s| !s.is_empty())
        .unwrap_or("start_job_run".to_string());
    let spark = |components: &[&str]| -> Vec<(String, String)> {
        components
            .iter()
            .map(|c| (naming("spark", c, account_id, encoded), namespace.clone()))
            .collect()
    };
    match t.as_str() {
        "interactive_endpoint" => spark(&["jeg", "jeg-kernel", "session"]),
        "spark_operator" => {
            let mut v = vec![(
                "emr-containers-sa-spark-operator".to_string(),
                operator_namespace,
            )];
            v.extend(spark(&["driver", "executor"]));
            v
        }
        "flink_operator" => {
            let mut v = vec![(
                "emr-containers-sa-flink-operator".to_string(),
                operator_namespace,
            )];
            v.extend(
                ["jobmanager", "taskmanager"]
                    .iter()
                    .map(|c| (naming("flink", c, account_id, encoded), namespace.clone())),
            );
            v
        }
        "livy" => vec![
            ("emr-containers-sa-livy".to_string(), operator_namespace),
            ("emr-container-sa-spark-livy".to_string(), namespace.clone()),
        ],
        _ => spark(&["client", "driver", "executor"]),
    }
}

/// `result["association"] if "association" in result else result`.
fn association(result: Value) -> Value {
    match result.get("association") {
        Some(a) => a.clone(),
        None => result,
    }
}

fn error_message(info: &crate::output::errors::ClientErrorInfo) -> String {
    info.parsed
        .get("Error")
        .and_then(|e| e.get("Message"))
        .map(crate::pyfmt::py_str)
        .unwrap_or_default()
}

/// `CreateRoleAssociationsCommand` / `DeleteRoleAssociationsCommand`.
fn role_associations(ctx: &Context, parsed: &Args, create: bool) -> Result<(), CliError> {
    let cluster_name = parsed.str("cluster_name").unwrap_or_default();
    let role_name = parsed.str("role_name").unwrap_or_default();
    let region = region(ctx);
    let eks_ctx = eks_context(ctx)?;
    let mut eks = Eks {
        client: Client::new(&eks_ctx, "eks")?,
        cluster: None,
    };
    let account_id = eks.account_id(&cluster_name)?;
    let role_arn = format!(
        "arn:{}:iam::{account_id}:role/{role_name}",
        util::policy_arn_suffix(&region)
    );
    let pairs = match parsed.str("service_account_name").filter(|s| !s.is_empty()) {
        Some(sa) => vec![(sa, parsed.str("namespace").unwrap_or_default())],
        None => mapping(parsed, &account_id, &base36(&role_name)),
    };
    let client = &eks.client;
    let mut results: Vec<Value> = Vec::new();
    for (service_account, namespace) in pairs {
        if create {
            let mut p = Map::new();
            p.insert("clusterName".to_string(), Value::str(cluster_name.clone()));
            p.insert("namespace".to_string(), Value::str(namespace.clone()));
            p.insert("roleArn".to_string(), Value::str(role_arn.clone()));
            p.insert(
                "serviceAccount".to_string(),
                Value::str(service_account.clone()),
            );
            match client.try_call("CreatePodIdentityAssociation", &Value::Map(p))? {
                CallResult::Ok(r) => results.push(association(r)),
                CallResult::ServiceError(info) if info.code == "ResourceInUseException" => {
                    basic::eprint(&format!(
                        "Skipping pod identity association creation because pod identity association already exists for service account {service_account} and role {role_name} in namespace {namespace}: {}\n",
                        error_message(&info)
                    ));
                }
                CallResult::ServiceError(info) => {
                    for r in &results {
                        basic::eprint(&format!(
                            "Rolling back association for service account {service_account} and role {role_name} in namespace {namespace} as an error was encountered\n"
                        ));
                        let mut p = Map::new();
                        p.insert(
                            "clusterName".to_string(),
                            r.get("clusterName").cloned().unwrap_or_default(),
                        );
                        p.insert(
                            "associationId".to_string(),
                            r.get("associationId").cloned().unwrap_or_default(),
                        );
                        client.call("DeletePodIdentityAssociation", &Value::Map(p))?;
                    }
                    return Err(general_error(
                        ctx,
                        &format!(
                            "Failed to create pod identity association for service account {service_account}, role {role_name} in namespace {namespace}: {}",
                            error_message(&info)
                        ),
                    ));
                }
            }
        } else {
            let fail = |info: &crate::output::errors::ClientErrorInfo| {
                general_error(
                    ctx,
                    &format!(
                        "Failed to delete pod identity association for service account {service_account}, role {role_name} in namespace {namespace}: {}",
                        error_message(info)
                    ),
                )
            };
            let mut p = Map::new();
            p.insert("clusterName".to_string(), Value::str(cluster_name.clone()));
            p.insert("namespace".to_string(), Value::str(namespace.clone()));
            p.insert(
                "serviceAccount".to_string(),
                Value::str(service_account.clone()),
            );
            let listed = match client.try_call("ListPodIdentityAssociations", &Value::Map(p))? {
                CallResult::Ok(v) => v,
                CallResult::ServiceError(info) => return Err(fail(&info)),
            };
            let first = listed
                .get("associations")
                .and_then(Value::as_list)
                .and_then(|l| l.first().cloned());
            let Some(first) = first else {
                basic::eprint(&format!(
                    "Skipping deletion as no pod identity association found for service account {service_account} and role {role_name} in namespace {namespace}\n"
                ));
                continue;
            };
            let mut p = Map::new();
            p.insert("clusterName".to_string(), Value::str(cluster_name.clone()));
            p.insert(
                "associationId".to_string(),
                first.get("associationId").cloned().unwrap_or_default(),
            );
            match client.try_call("DeletePodIdentityAssociation", &Value::Map(p))? {
                CallResult::Ok(r) => results.push(association(r)),
                CallResult::ServiceError(info) => return Err(fail(&info)),
            }
        }
    }
    if !results.is_empty() {
        basic::print(&util::json_dumps(&Value::List(results), 4));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base36_matches_reference() {
        assert_eq!(base36(""), "0");
        assert_eq!(base36("a"), "2p");
        assert_eq!(base36("MyRole"), "u70qubg4l");
    }
}
