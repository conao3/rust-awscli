//! `deploy push`, `register`, `deregister`, `install`, and `uninstall`
//! (`awscli/customizations/codedeploy/`).

use std::path::Path;

use regex::Regex;
use serde_json::json;

use super::zipfile::{ZipWriter, walk};
use crate::cli::argparse::NsVal;
use crate::commands::services::basic::{self, ArgDef};
use crate::commands::services::exec::{Client, sub_context};
use crate::driver::{CallResult, Context};
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "push" => push(ctx, args),
        "register" => register(ctx, args),
        "deregister" => deregister(ctx, args),
        "install" => install(ctx, args),
        "uninstall" => uninstall(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `DEFAULT_CONFIG_FILE` (`codedeploy/systems.py`).
const DEFAULT_CONFIG_FILE: &str = "codedeploy.onpremises.yml";
const MULTIPART_LIMIT: usize = 6 << 20;

fn map(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Map(m)
}

fn general(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

/// `os.path.abspath`.
pub(crate) fn abspath(path: &str) -> String {
    let joined = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        format!("{cwd}/{path}")
    };
    let norm = super::zipfile::normalize_arcname(&joined);
    format!("/{}", norm.trim_start_matches('/'))
        .trim_end_matches("/.")
        .to_string()
}

/// `validate_s3_location`: (bucket, key) of an `s3://bucket/key` value.
fn validate_s3_location(
    ctx: &Context,
    value: Option<&str>,
    arg_name: &str,
) -> Result<Option<(String, String)>, CliError> {
    let Some(v) = value.filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let re = Regex::new(r"^s3://(.+?)/(.+)").expect("regex");
    match re.captures(v) {
        Some(c) => Ok(Some((c[1].to_string(), c[2].to_string()))),
        None => Err(basic::param_validation(
            ctx,
            &format!("--{arg_name} must specify the Amazon S3 URL format as s3://<bucket>/<key>."),
        )),
    }
}

/// `validate_region`.
fn validate_region(ctx: &Context) -> Result<String, CliError> {
    match ctx.globals.region.clone().or_else(|| ctx.session.region()) {
        Some(r) if !r.is_empty() => Ok(r),
        _ => Err(ctx.structured_error(ExitCode::Config, "Configuration", "Region not specified.")),
    }
}

/// `str(e)` of an exception raised by a client call.
fn call_error_text(result: CallResult) -> Result<Value, String> {
    match result {
        CallResult::Ok(v) => Ok(v),
        CallResult::ServiceError(info) => Err(info.exception_message()),
    }
}

/// A client call whose failures are kept as `str(exception)`: the
/// `ParamValidationError` report, `str(ClientError)`, or the error text.
pub(crate) fn raw_call(
    client: &Client<'_>,
    op: &str,
    params: &Value,
) -> Result<CallResult, String> {
    client.call_raw(op, params).map_err(|e| {
        if let Some(p) = e.downcast_ref::<crate::handlers::ParamValidationError>() {
            return p.to_string();
        }
        client
            .ctx
            .general_error(&e)
            .message
            .trim()
            .trim_start_matches("awsr: [ERROR]: ")
            .to_string()
    })
}

/// [`raw_call`] with a service error as `str(ClientError)`.
pub(crate) fn try_call(client: &Client<'_>, op: &str, params: &Value) -> Result<Value, String> {
    call_error_text(raw_call(client, op, params)?)
}

/// `os.walk` + the zip of `Push._compress`.
fn compress(source: &str, ignore_hidden: bool) -> Result<Vec<u8>, String> {
    let source_path = abspath(source);
    let appspec_path = format!("{source_path}/appspec.yml");
    let filter = |n: &str| !(ignore_hidden && n.starts_with('.'));
    let mut zip = ZipWriter::new();
    let mut contains_appspec = false;
    for (root, _, files) in walk(Path::new(source), &filter) {
        for f in files {
            let filename = abspath(&root.join(&f).display().to_string());
            let arcname = filename
                .get(source_path.len() + 1..)
                .unwrap_or_default()
                .to_string();
            if filename == appspec_path {
                contains_appspec = true;
            }
            zip.write_file(Path::new(&filename), &arcname)
                .map_err(|e| e.to_string())?;
        }
    }
    let bytes = zip.finish();
    if !contains_appspec {
        return Err(format!("{appspec_path} was not found"));
    }
    Ok(bytes)
}

/// `Push`.
fn push(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("application-name").required(),
        ArgDef::new("s3-location").required(),
        ArgDef::new("ignore-hidden-files")
            .flag()
            .default(NsVal::Bool(false)),
        ArgDef::new("no-ignore-hidden-files")
            .flag()
            .default(NsVal::Bool(false)),
        ArgDef::new("source").default_str("."),
        ArgDef::new("description"),
    ];
    let Some(a) = basic::parse(ctx, &["deploy", "push"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let s3_location = a.str("s3-location").unwrap_or_default();
    let (bucket, key) =
        validate_s3_location(ctx, Some(&s3_location), "s3-location")?.expect("required");
    if a.bool("ignore-hidden-files") && a.bool("no-ignore-hidden-files") {
        return Err(basic::param_validation(
            ctx,
            "You cannot specify both --ignore-hidden-files and --no-ignore-hidden-files.",
        ));
    }
    let description = match a.str("description").filter(|d| !d.is_empty()) {
        Some(d) => d,
        None => {
            let now = chrono::Utc::now().naive_utc();
            let iso = if now.and_utc().timestamp_subsec_micros() == 0 {
                now.format("%Y-%m-%dT%H:%M:%S").to_string()
            } else {
                now.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
            };
            format!("Uploaded by AWS CLI {iso} UTC")
        }
    };
    let codedeploy = Client::new(ctx, "codedeploy")?;
    let s3_ctx = sub_context(ctx, None, None, None)?;
    let s3 = Client::new(&s3_ctx, "s3")?;
    let source = a.str("source").unwrap_or_else(|| ".".to_string());
    let bundle = compress(&source, a.bool("ignore-hidden-files")).map_err(|e| general(ctx, &e))?;
    let upload = upload_to_s3(&s3, &bucket, &key, bundle).map_err(|e| {
        general(
            ctx,
            &format!("Failed to upload '{source}' to '{s3_location}': {e}"),
        )
    })?;
    let etag = upload
        .get("ETag")
        .and_then(Value::as_str)
        .map(|e| e.replace('"', ""))
        .ok_or_else(|| {
            general(
                ctx,
                &format!("Failed to upload '{source}' to '{s3_location}': 'ETag'"),
            )
        })?;
    let version = upload.get("VersionId").map(crate::pyfmt::py_str);
    let mut location = vec![
        ("bucket", Value::str(&bucket)),
        ("key", Value::str(&key)),
        ("bundleType", Value::str("zip")),
        ("eTag", Value::str(&etag)),
    ];
    if let Some(v) = &version {
        location.push(("version", Value::str(v)));
    }
    let application_name = a.str("application-name").unwrap_or_default();
    codedeploy.call(
        "RegisterApplicationRevision",
        &map(vec![
            ("applicationName", Value::str(&application_name)),
            (
                "revision",
                map(vec![
                    ("revisionType", Value::str("S3")),
                    ("s3Location", map(location)),
                ]),
            ),
            ("description", Value::str(&description)),
        ]),
    )?;
    let version_string = version.map(|v| format!(",version={v}")).unwrap_or_default();
    basic::print(&format!(
        "To deploy with this revision, run:\naws deploy create-deployment --application-name {application_name} --s3-location bucket={bucket},key={key},bundleType=zip,eTag={etag}{version_string} --deployment-group-name <deployment-group-name> --deployment-config-name <deployment-config-name> --description <description>\n"
    ));
    Ok(ExitCode::Success)
}

/// `_upload_to_s3` / `_multipart_upload_to_s3`; errors are
/// `str(exception)`.
fn upload_to_s3(
    s3: &Client<'_>,
    bucket: &str,
    key: &str,
    bundle: Vec<u8>,
) -> Result<Value, String> {
    let call = |op: &str, params: Value| -> Result<Value, String> { try_call(s3, op, &params) };
    if bundle.len() < MULTIPART_LIMIT {
        return call(
            "PutObject",
            map(vec![
                ("Bucket", Value::str(bucket)),
                ("Key", Value::str(key)),
                ("Body", Value::Bytes(bundle)),
            ]),
        );
    }
    let created = call(
        "CreateMultipartUpload",
        map(vec![
            ("Bucket", Value::str(bucket)),
            ("Key", Value::str(key)),
        ]),
    )?;
    let upload_id = created
        .get("UploadId")
        .map(crate::pyfmt::py_str)
        .ok_or_else(|| "'UploadId'".to_string())?;
    let mut parts = Vec::new();
    let result = (|| {
        for (i, chunk) in bundle.chunks(MULTIPART_LIMIT).enumerate() {
            let part = (i + 1) as i64;
            let r = call(
                "UploadPart",
                map(vec![
                    ("Bucket", Value::str(bucket)),
                    ("Key", Value::str(key)),
                    ("UploadId", Value::str(&upload_id)),
                    ("PartNumber", Value::int(part)),
                    ("Body", Value::Bytes(chunk.to_vec())),
                ]),
            )?;
            parts.push(map(vec![
                ("PartNumber", Value::int(part)),
                ("ETag", r.get("ETag").cloned().unwrap_or(Value::Null)),
            ]));
        }
        call(
            "CompleteMultipartUpload",
            map(vec![
                ("Bucket", Value::str(bucket)),
                ("Key", Value::str(key)),
                ("UploadId", Value::str(&upload_id)),
                ("MultipartUpload", map(vec![("Parts", Value::List(parts))])),
            ]),
        )
    })();
    if result.is_err() {
        let _ = call(
            "AbortMultipartUpload",
            map(vec![
                ("Bucket", Value::str(bucket)),
                ("Key", Value::str(key)),
                ("UploadId", Value::str(&upload_id)),
            ]),
        );
    }
    result
}

fn instance_name_arg() -> ArgDef {
    ArgDef::new("instance-name").required()
}

/// `validate_instance_name`.
fn validate_instance_name(ctx: &Context, name: Option<&str>) -> Result<(), CliError> {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Ok(());
    };
    if !Regex::new(r"^[A-Za-z0-9+=,.@_-]+$")
        .expect("regex")
        .is_match(name)
    {
        return Err(basic::param_validation(
            ctx,
            "Instance name contains invalid characters.",
        ));
    }
    if name.starts_with("i-") {
        return Err(basic::param_validation(
            ctx,
            "Instance name cannot start with 'i-'.",
        ));
    }
    if name.chars().count() > 100 {
        return Err(basic::param_validation(
            ctx,
            "Instance name cannot be longer than 100 characters.",
        ));
    }
    Ok(())
}

/// The `except Exception` of the on-premises commands.
fn failure(text: &str, instructions: &str) -> CliError {
    CliError::new(
        ExitCode::General,
        format!("ERROR\n{text}\n{instructions}\n"),
    )
}

const REGISTER_HELP: &str = "Register the on-premises instance by following the instructions in \"Configure Existing On-Premises Instances by Using AWS CodeDeploy\" in the AWS CodeDeploy User Guide.";

/// `Register`.
fn register(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        instance_name_arg(),
        ArgDef::new("tags").schema(json!({
            "type": "array",
            "items": {"type": "object", "properties": {
                "Key": {"description": "The tag key.", "type": "string", "required": true},
                "Value": {"description": "The tag value.", "type": "string", "required": true}
            }}
        })),
        ArgDef::new("iam-user-arn"),
    ];
    let Some(a) = basic::parse(ctx, &["deploy", "register"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let region = validate_region(ctx)?;
    let instance_name = a.str("instance-name").unwrap_or_default();
    validate_instance_name(ctx, Some(&instance_name))?;
    let tags = a.list("tags").unwrap_or_default();
    if !tags.is_empty() {
        if tags.len() > 10 {
            return Err(basic::param_validation(
                ctx,
                "Instances can only have a maximum of 10 tags.",
            ));
        }
        for t in &tags {
            let len = |k: &str| {
                t.get(k)
                    .and_then(Value::as_str)
                    .map_or(0, |s| s.chars().count())
            };
            if len("Key") > 128 {
                return Err(basic::param_validation(
                    ctx,
                    "Tag Key cannot be longer than 128 characters.",
                ));
            }
            if len("Value") > 256 {
                return Err(basic::param_validation(
                    ctx,
                    "Tag Value cannot be longer than 256 characters.",
                ));
            }
        }
    }
    let mut iam_user_arn = a.str("iam-user-arn").filter(|s| !s.is_empty());
    if let Some(arn) = &iam_user_arn
        && !Regex::new(r"^arn:aws:iam::[0-9]{12}:user/[A-Za-z0-9/+=,.@_-]+$")
            .expect("regex")
            .is_match(arn)
    {
        return Err(basic::param_validation(ctx, "Invalid IAM user ARN."));
    }
    let codedeploy_ctx = sub_context(
        ctx,
        ctx.globals.endpoint_url.clone(),
        Some(region.clone()),
        None,
    )?;
    let codedeploy = Client::new(&codedeploy_ctx, "codedeploy")?;
    let iam_ctx = sub_context(ctx, None, Some(region.clone()), None)?;
    let iam = Client::new(&iam_ctx, "iam")?;
    let mut steps = || -> Result<(), String> {
        let call =
            |c: &Client<'_>, op: &str, p: Value| -> Result<Value, String> { try_call(c, op, &p) };
        let key_error = |k: &str| format!("'{k}'");
        if iam_user_arn.is_none() {
            basic::print("Creating the IAM user... ");
            let r = call(
                &iam,
                "CreateUser",
                map(vec![
                    ("Path", Value::str("/AWS/CodeDeploy/")),
                    ("UserName", Value::str(&instance_name)),
                ]),
            )?;
            let arn = r
                .get("User")
                .ok_or_else(|| key_error("User"))?
                .get("Arn")
                .map(crate::pyfmt::py_str)
                .ok_or_else(|| key_error("Arn"))?;
            basic::print(&format!("DONE\nIamUserArn: {arn}\n"));
            iam_user_arn = Some(arn.clone());
            basic::print("Creating the IAM user access key... ");
            let r = call(
                &iam,
                "CreateAccessKey",
                map(vec![("UserName", Value::str(&instance_name))]),
            )?;
            let key = r.get("AccessKey").ok_or_else(|| key_error("AccessKey"))?;
            let access_key_id = key
                .get("AccessKeyId")
                .map(crate::pyfmt::py_str)
                .ok_or_else(|| key_error("AccessKeyId"))?;
            let secret = key
                .get("SecretAccessKey")
                .map(crate::pyfmt::py_str)
                .ok_or_else(|| key_error("SecretAccessKey"))?;
            basic::print(&format!(
                "DONE\nAccessKeyId: {access_key_id}\nSecretAccessKey: {secret}\n"
            ));
            basic::print("Creating the IAM user policy... ");
            let document = "{\n    \"Version\": \"2012-10-17\",\n    \"Statement\": [ {\n        \"Action\": [ \"s3:Get*\", \"s3:List*\" ],\n        \"Effect\": \"Allow\",\n        \"Resource\": \"*\"\n    } ]\n}";
            call(
                &iam,
                "PutUserPolicy",
                map(vec![
                    ("UserName", Value::str(&instance_name)),
                    ("PolicyName", Value::str("codedeploy-agent")),
                    ("PolicyDocument", Value::str(document)),
                ]),
            )?;
            basic::print(&format!(
                "DONE\nPolicyName: codedeploy-agent\nPolicyDocument: {document}\n"
            ));
            basic::print(&format!(
                "Creating the on-premises instance configuration file named {DEFAULT_CONFIG_FILE}..."
            ));
            write_config(&format!(
                "---\nregion: {region}\niam_user_arn: {arn}\naws_access_key_id: {access_key_id}\naws_secret_access_key: {secret}\n"
            ))
            .map_err(|e| format!("Failed to create config file {DEFAULT_CONFIG_FILE}: {e}"))?;
            basic::print("DONE\n");
        }
        basic::print("Registering the on-premises instance... ");
        call(
            &codedeploy,
            "RegisterOnPremisesInstance",
            map(vec![
                ("instanceName", Value::str(&instance_name)),
                (
                    "iamUserArn",
                    Value::str(iam_user_arn.clone().unwrap_or_default()),
                ),
            ]),
        )?;
        basic::print("DONE\n");
        if !tags.is_empty() {
            basic::print("Adding tags to the on-premises instance... ");
            call(
                &codedeploy,
                "AddTagsToOnPremisesInstances",
                map(vec![
                    ("tags", Value::List(tags.clone())),
                    (
                        "instanceNames",
                        Value::List(vec![Value::str(&instance_name)]),
                    ),
                ]),
            )?;
            basic::print("DONE\n");
        }
        basic::print(&format!(
            "Copy the on-premises configuration file named {DEFAULT_CONFIG_FILE} to the on-premises instance, and run the following command on the on-premises instance to install and configure the AWS CodeDeploy Agent:\naws deploy install --config-file {DEFAULT_CONFIG_FILE}\n"
        ));
        Ok(())
    };
    match steps() {
        Ok(()) => Ok(ExitCode::Success),
        Err(text) => Err(failure(&text, REGISTER_HELP)),
    }
}

/// `os.open(DEFAULT_CONFIG_FILE, O_WRONLY | O_CREAT | O_TRUNC, 0o600)`.
fn write_config(text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(DEFAULT_CONFIG_FILE)
        .map_err(|e| {
            std::io::Error::new(
                e.kind(),
                crate::cli::paramfile::os_error(&e, DEFAULT_CONFIG_FILE),
            )
        })?;
    std::fs::set_permissions(DEFAULT_CONFIG_FILE, std::fs::Permissions::from_mode(0o600))?;
    f.write_all(text.as_bytes())
}

const DEREGISTER_HELP: &str = "Deregister the on-premises instance by following the instructions in \"Configure Existing On-Premises Instances by Using AWS CodeDeploy\" in the AWS CodeDeploy User Guide.";

/// `Deregister`.
fn deregister(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        instance_name_arg(),
        ArgDef::new("no-delete-iam-user")
            .flag()
            .default(NsVal::Bool(false)),
    ];
    let Some(a) = basic::parse(ctx, &["deploy", "deregister"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let region = validate_region(ctx)?;
    let instance_name = a.str("instance-name").unwrap_or_default();
    validate_instance_name(ctx, Some(&instance_name))?;
    let codedeploy_ctx = sub_context(
        ctx,
        ctx.globals.endpoint_url.clone(),
        Some(region.clone()),
        None,
    )?;
    let codedeploy = Client::new(&codedeploy_ctx, "codedeploy")?;
    let iam_ctx = sub_context(ctx, None, Some(region), None)?;
    let iam = Client::new(&iam_ctx, "iam")?;
    let no_delete = a.bool("no-delete-iam-user");
    let steps = || -> Result<(), String> {
        let raw = |c: &Client<'_>, op: &str, p: Value| -> Result<CallResult, String> {
            raw_call(c, op, &p)
        };
        let call = |c: &Client<'_>, op: &str, p: Value| -> Result<Value, String> {
            call_error_text(raw(c, op, p)?)
        };
        let key_error = |k: &str| format!("'{k}'");
        basic::print("Retrieving on-premises instance information... ");
        let r = call(
            &codedeploy,
            "GetOnPremisesInstance",
            map(vec![("instanceName", Value::str(&instance_name))]),
        )?;
        let info = r
            .get("instanceInfo")
            .ok_or_else(|| key_error("instanceInfo"))?;
        let arn = info
            .get("iamUserArn")
            .map(crate::pyfmt::py_str)
            .ok_or_else(|| key_error("iamUserArn"))?;
        let user_name = arn[arn.rfind('/').map_or(0, |i| i + 1)..].to_string();
        let tags = info.get("tags").ok_or_else(|| key_error("tags"))?.clone();
        basic::print(&format!("DONE\nIamUserArn: {arn}\n"));
        let tag_list = tags.as_list().cloned().unwrap_or_default();
        if !tag_list.is_empty() {
            let mut line = String::from("Tags:");
            for t in &tag_list {
                line.push_str(&format!(
                    " Key={},Value={}",
                    t.get("Key").map(crate::pyfmt::py_str).unwrap_or_default(),
                    t.get("Value").map(crate::pyfmt::py_str).unwrap_or_default()
                ));
            }
            line.push('\n');
            basic::print(&line);
            basic::print("Removing tags from the on-premises instance... ");
            call(
                &codedeploy,
                "RemoveTagsFromOnPremisesInstances",
                map(vec![
                    ("tags", tags.clone()),
                    (
                        "instanceNames",
                        Value::List(vec![Value::str(&instance_name)]),
                    ),
                ]),
            )?;
            basic::print("DONE\n");
        }
        basic::print("Deregistering the on-premises instance... ");
        call(
            &codedeploy,
            "DeregisterOnPremisesInstance",
            map(vec![("instanceName", Value::str(&instance_name))]),
        )?;
        basic::print("DONE\n");
        if !no_delete {
            let ignore_missing = |r: CallResult| -> Result<Option<Value>, String> {
                match r {
                    CallResult::Ok(v) => Ok(Some(v)),
                    CallResult::ServiceError(info) if info.code == "NoSuchEntity" => Ok(None),
                    CallResult::ServiceError(info) => Err(info.exception_message()),
                }
            };
            for (title, list_op, key, delete_op, item_key, param) in [
                (
                    "Deleting the IAM user policies... ",
                    "ListUserPolicies",
                    "PolicyNames",
                    "DeleteUserPolicy",
                    None,
                    "PolicyName",
                ),
                (
                    "Deleting the IAM user access keys... ",
                    "ListAccessKeys",
                    "AccessKeyMetadata",
                    "DeleteAccessKey",
                    Some("AccessKeyId"),
                    "AccessKeyId",
                ),
            ] {
                basic::print(title);
                let mut marker: Option<Value> = None;
                'pages: loop {
                    let mut p = vec![("UserName", Value::str(&user_name))];
                    if let Some(m) = &marker {
                        p.push(("Marker", m.clone()));
                    }
                    let Some(page) = ignore_missing(raw(&iam, list_op, map(p))?)? else {
                        break;
                    };
                    for item in page
                        .get(key)
                        .and_then(Value::as_list)
                        .cloned()
                        .unwrap_or_default()
                    {
                        let v = match item_key {
                            Some(k) => item.get(k).cloned().unwrap_or(Value::Null),
                            None => item,
                        };
                        if ignore_missing(raw(
                            &iam,
                            delete_op,
                            map(vec![("UserName", Value::str(&user_name)), (param, v)]),
                        )?)?
                        .is_none()
                        {
                            break 'pages;
                        }
                    }
                    match page.get("Marker") {
                        Some(m) if page.get("IsTruncated").is_some_and(Value::is_truthy) => {
                            marker = Some(m.clone());
                        }
                        _ => break,
                    }
                }
                basic::print("DONE\n");
            }
            basic::print(&format!("Deleting the IAM user ({user_name})... "));
            ignore_missing(raw(
                &iam,
                "DeleteUser",
                map(vec![("UserName", Value::str(&user_name))]),
            )?)?;
            basic::print("DONE\n");
        }
        basic::print(
            "Run the following command on the on-premises instance to uninstall the codedeploy-agent:\naws deploy uninstall\n",
        );
        Ok(())
    };
    match steps() {
        Ok(()) => Ok(ExitCode::Success),
        Err(text) => Err(failure(&text, DEREGISTER_HELP)),
    }
}

/// The on-premises systems of `codedeploy/systems.py` this host can be.
#[derive(Clone, Copy, PartialEq)]
enum System {
    Ubuntu,
    Rhel,
}

const CONFIG_DIR: &str = "/etc/codedeploy-agent/conf";

fn config_path() -> String {
    format!("{CONFIG_DIR}/{DEFAULT_CONFIG_FILE}")
}

fn makedirs(name: &std::path::Path) -> Result<(), String> {
    if let Some(head) = name.parent()
        && !head.as_os_str().is_empty()
        && !head.exists()
    {
        makedirs(head)?;
    }
    match std::fs::create_dir(name) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            Err(crate::cli::paramfile::os_error(&e, &name.to_string_lossy()))
        }
        _ => Ok(()),
    }
}

/// `awscli.compat.linux_distribution()[0]` from `/etc/lsb-release` or
/// the first `/etc/*-release` file of a supported distribution.
fn linux_distribution() -> String {
    if let Ok(text) = std::fs::read_to_string("/etc/lsb-release") {
        let mut name = None;
        let mut version = None;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("DISTRIB_ID=") {
                name = Some(v.trim().to_string());
            }
            if let Some(v) = line.strip_prefix("DISTRIB_RELEASE=") {
                version = Some(v.trim().to_string());
            }
        }
        if let (Some(n), Some(_)) = (name, version) {
            return n;
        }
    }
    let supported = [
        "SuSE",
        "debian",
        "fedora",
        "redhat",
        "centos",
        "mandrake",
        "mandriva",
        "rocks",
        "slackware",
        "yellowdog",
        "gentoo",
        "UnitedLinux",
        "turbolinux",
        "arch",
        "mageia",
    ];
    let re = Regex::new(r"^(\w+)[-_](release|version)$").expect("regex");
    let mut names: Vec<String> = std::fs::read_dir("/etc")
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let lsb = Regex::new(r"(.+) release ([\d.]+)[^(]*(?:\((.+)\))?").expect("regex");
    for n in names {
        if let Some(c) = re.captures(&n)
            && supported.contains(&&c[1])
        {
            let first = std::fs::read_to_string(format!("/etc/{n}"))
                .ok()
                .and_then(|t| t.lines().next().map(str::to_string))
                .unwrap_or_default();
            return match lsb.captures(&first) {
                Some(c) => c[1].to_string(),
                None => c[1].to_string(),
            };
        }
    }
    String::new()
}

/// `validate_instance` and `System.validate_administrator`.
fn validate_instance(ctx: &Context) -> Result<System, CliError> {
    let distribution = linux_distribution();
    let system = if distribution.contains("Red Hat Enterprise Linux Server") {
        Some(System::Rhel)
    } else if distribution.contains("Ubuntu") {
        Some(System::Ubuntu)
    } else {
        None
    };
    let Some(system) = system else {
        return Err(general(
            ctx,
            "Only Ubuntu Server, Red Hat Enterprise Linux Server and Windows Server operating systems are supported.",
        ));
    };
    let imds = || -> std::io::Result<bool> {
        use std::io::{BufRead, Write};
        let timeout = Some(std::time::Duration::from_secs(1));
        let mut stream = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([169, 254, 169, 254], 80)),
            std::time::Duration::from_secs(1),
        )?;
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(timeout)?;
        stream.write_all(
            b"GET /latest/meta-data/ HTTP/1.1\r\nHost: 169.254.169.254\r\nConnection: close\r\n\r\n",
        )?;
        let mut status = String::new();
        std::io::BufReader::new(stream).read_line(&mut status)?;
        Ok(status
            .split(' ')
            .nth(1)
            .and_then(|c| c.parse::<u16>().ok())
            .is_some_and(|c| c < 400))
    };
    if imds().unwrap_or(false) {
        return Err(general(ctx, "Amazon EC2 instances are not supported."));
    }
    if unsafe { libc::geteuid() } != 0 {
        return Err(general(ctx, "You must run this command as sudo."));
    }
    Ok(system)
}

/// `subprocess.check_call(argv)`.
fn check_call(argv: &[&str], env: Option<&[(String, String)]>) -> Result<(), String> {
    let mut cmd = std::process::Command::new(argv[0]);
    cmd.args(&argv[1..]);
    if let Some(env) = env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    let status = cmd
        .status()
        .map_err(|e| crate::cli::paramfile::os_error(&e, argv[0]))?;
    if status.success() {
        return Ok(());
    }
    let quoted: Vec<String> = argv.iter().map(|a| format!("'{a}'")).collect();
    Err(format!(
        "Command '[{}]' returned non-zero exit status {}.",
        quoted.join(", "),
        status.code().unwrap_or(-1)
    ))
}

/// `Linux._stop_agent` with the system's not-found message.
fn stop_agent(system: System) -> Result<bool, String> {
    let not_found = match system {
        System::Ubuntu => "codedeploy-agent: unrecognized service",
        System::Rhel => "Redirecting to /bin/systemctl stop  codedeploy-agent.service",
    };
    let out = std::process::Command::new("service")
        .args(["codedeploy-agent", "stop"])
        .output()
        .map_err(|e| crate::cli::paramfile::os_error(&e, "service"))?;
    let error = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() && !error.contains(not_found) {
        return Err(format!(
            "Failed to stop the AWS CodeDeploy Agent:\n{}",
            crate::pyfmt::bytes_repr(&out.stderr)
        ));
    }
    Ok(out.status.success())
}

const INSTALL_HELP: &str = "Install the AWS CodeDeploy Agent on the on-premises instance by following the instructions in \"Configure Existing On-Premises Instances by Using AWS CodeDeploy\" in the AWS CodeDeploy User Guide.";

/// `Install`.
fn install(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("config-file").required(),
        ArgDef::new("override-config")
            .flag()
            .default(NsVal::Bool(false)),
        ArgDef::new("agent-installer"),
    ];
    let Some(a) = basic::parse(ctx, &["deploy", "install"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let region = validate_region(ctx)?;
    let system = validate_instance(ctx)?;
    if Path::new(&config_path()).is_file() && !a.bool("override-config") {
        return Err(general(
            ctx,
            "The on-premises instance configuration file already exists. Specify --override-config to update the existing on-premises instance configuration file.",
        ));
    }
    let (bucket, key, installer) =
        match validate_s3_location(ctx, a.str("agent-installer").as_deref(), "agent-installer")? {
            Some((b, k)) => {
                let installer = k[k.rfind('/').map_or(0, |i| i + 1)..].to_string();
                (b, k, installer)
            }
            None => (
                format!("aws-codedeploy-{region}"),
                "latest/install".to_string(),
                "install".to_string(),
            ),
        };
    let s3_ctx = sub_context(ctx, None, Some(region.clone()), None)?;
    let s3 = Client::new(&s3_ctx, "s3")?;
    let config_file = a.str("config-file").unwrap_or_default();
    let steps = || -> Result<(), String> {
        basic::print("Creating the on-premises instance configuration file... ");
        makedirs(std::path::Path::new(CONFIG_DIR))?;
        if config_file != config_path() {
            use std::io::Write;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let contents = std::fs::read(&config_file)
                .map_err(|e| crate::cli::paramfile::os_error(&e, &config_file))?;
            let dest = config_path();
            let os_error = |e: std::io::Error| crate::cli::paramfile::os_error(&e, &dest);
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&dest)
                .map_err(os_error)?;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600))
                .map_err(os_error)?;
            f.write_all(&contents).map_err(os_error)?;
        }
        basic::print("DONE\n");
        basic::print("Installing the AWS CodeDeploy Agent... ");
        match system {
            System::Ubuntu => {
                check_call(&["apt-get", "-y", "update"], None)?;
                check_call(&["apt-get", "-y", "install", "ruby2.0"], None)?;
            }
            System::Rhel => check_call(&["yum", "-y", "install", "ruby"], None)?,
        }
        stop_agent(system)?;
        let r = try_call(
            &s3,
            "GetObject",
            &map(vec![
                ("Bucket", Value::str(&bucket)),
                ("Key", Value::str(&key)),
            ]),
        )?;
        let body = match r.get("Body") {
            Some(Value::Bytes(b)) => b.clone(),
            Some(Value::Str(s)) => s.clone().into_bytes(),
            _ => Vec::new(),
        };
        std::fs::write(&installer, body)
            .map_err(|e| crate::cli::paramfile::os_error(&e, &installer))?;
        check_call(&["chmod", "+x", &format!("./{installer}")], None)?;
        let creds = ctx
            .runtime
            .block_on(ctx.session.credentials(Some(&region)))
            .ok()
            .flatten();
        let mut env = vec![("AWS_REGION".to_string(), region.clone())];
        if let Some(c) = creds {
            env.push(("AWS_ACCESS_KEY_ID".to_string(), c.access_key.clone()));
            env.push(("AWS_SECRET_ACCESS_KEY".to_string(), c.secret_key.clone()));
            if let Some(t) = &c.token {
                env.push(("AWS_SESSION_TOKEN".to_string(), t.clone()));
            }
        }
        check_call(&[&format!("./{installer}"), "auto"], Some(&env))?;
        basic::print("DONE\n");
        Ok(())
    };
    match steps() {
        Ok(()) => Ok(ExitCode::Success),
        Err(text) => Err(failure(&text, INSTALL_HELP)),
    }
}

const UNINSTALL_HELP: &str = "Uninstall the AWS CodeDeploy Agent on the on-premises instance by following the instructions in \"Configure Existing On-Premises Instances by Using AWS CodeDeploy\" in the AWS CodeDeploy User Guide.";

/// `Uninstall`.
fn uninstall(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    if basic::parse(ctx, &["deploy", "uninstall"], &[], args)?.is_none() {
        return Ok(ExitCode::Success);
    }
    validate_region(ctx)?;
    let system = validate_instance(ctx)?;
    let steps = || -> Result<(), String> {
        basic::print("Uninstalling the AWS CodeDeploy Agent... ");
        if stop_agent(system)? {
            match system {
                System::Ubuntu => check_call(&["dpkg", "-r", "codedeploy-agent"], None)?,
                System::Rhel => check_call(&["yum", "-y", "erase", "codedeploy-agent"], None)?,
            }
        }
        basic::print("DONE\n");
        basic::print("Deleting the on-premises instance configuration... ");
        match std::fs::remove_file(config_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.to_string()),
            _ => {}
        }
        basic::print("DONE\n");
        Ok(())
    };
    match steps() {
        Ok(()) => Ok(ExitCode::Success),
        Err(text) => Err(failure(&text, UNINSTALL_HELP)),
    }
}
