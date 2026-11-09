//! `servicecatalog generate product` and
//! `servicecatalog generate provisioning-artifact`
//! (`awscli/customizations/servicecatalog/`).

use serde_json::json;

use super::basic::{self, ArgDef, Args};
use super::exec::Client;
use crate::cli::argparse::{Action, Nargs, NsVal, Parsed, Parser};
use crate::cli::render::argparse_error;
use crate::driver::{CallResult, Context, client_error};
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "generate" => generate(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

const SUBCOMMANDS: [&str; 2] = ["product", "provisioning-artifact"];

/// `GenerateCommand` (a `BasicCommand` with `SUBCOMMANDS`).
fn generate(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let render = |f| crate::cli::render_fail(f, &ctx.globals, &ctx.session);
    if args.len() == 1 && args[0] == "help" {
        let mut out = String::from("generate\n\nAvailable commands:\n\n");
        for s in SUBCOMMANDS {
            out.push_str(&format!("  * {s}\n"));
        }
        crate::driver::write_stdout(ctx, out.as_bytes())?;
        return Ok(ExitCode::Success);
    }
    let mut parser = Parser::new();
    let mut sub = Action::positional("subcommand", Nargs::Optional);
    sub.choices = Some(SUBCOMMANDS.iter().map(|s| s.to_string()).collect());
    parser.add(sub);
    let (ns, remaining) = match parser
        .parse_known_args(args)
        .map_err(|e| render(argparse_error(&e.to_string())))?
    {
        Parsed::Args { ns, extras } => (ns, extras),
        Parsed::Version => unreachable!("no version action"),
    };
    match ns.get("subcommand") {
        Some(NsVal::Str(s)) => {
            let mut rest = args.to_vec();
            if let Some(i) = rest.iter().position(|a| a == s) {
                rest.remove(i);
            }
            match s.as_str() {
                "product" => product(ctx, &rest),
                _ => provisioning_artifact(ctx, &rest),
            }
        }
        _ => {
            if !remaining.is_empty() {
                return Err(render(crate::cli::Fail::ParamValidation(
                    crate::cli::format_unknown_options_error(
                        &remaining,
                        &crate::cli::option_suggestion_candidates(None),
                    ),
                )));
            }
            Err(basic::usage_error(ctx, &["servicecatalog", "generate"]))
        }
    }
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Map(m)
}

fn artifact_type_arg() -> ArgDef {
    ArgDef::new("provisioning-artifact-type")
        .required()
        .choices(&[
            "CLOUD_FORMATION_TEMPLATE",
            "MARKETPLACE_AMI",
            "MARKETPLACE_CAR",
        ])
}

/// `json.dumps(value, indent=2, ensure_ascii=False)`.
fn json_dumps_indent2(value: &Value) -> String {
    crate::pyfmt::json_dumps_pretty(value)
        .lines()
        .map(|l| {
            let n = l.len() - l.trim_start_matches(' ').len();
            format!("{}{}", " ".repeat(n / 2), &l[n..])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `session.get_available_regions('servicecatalog')`.
fn available_regions() -> Vec<String> {
    let endpoints = crate::model::endpoints();
    let Some(partitions) = endpoints["partitions"].as_array() else {
        return Vec::new();
    };
    let Some(aws) = partitions.iter().find(|p| p["partition"] == "aws") else {
        return Vec::new();
    };
    let regions = &aws["regions"];
    aws["services"]["servicecatalog"]["endpoints"]
        .as_object()
        .map(|e| {
            e.keys()
                .filter(|k| regions.get(k).is_some())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `GenerateBaseCommand.get_and_validate_region`.
fn validate_region(ctx: &Context) -> Result<String, CliError> {
    let region = ctx.globals.region.clone().or_else(|| ctx.session.region());
    match region {
        Some(r) if available_regions().contains(&r) => Ok(r),
        _ => Err(basic::param_validation(
            ctx,
            &format!(
                "An error occurred (InvalidParametersException) : Region {} is not supported",
                ctx.globals.region.as_deref().unwrap_or("None")
            ),
        )),
    }
}

/// `GenerateBaseCommand._run_main`: upload the template with
/// `S3Uploader(force_upload=True)`; returns the template URL.
fn upload_template(ctx: &Context, a: &Args) -> Result<String, CliError> {
    let region = validate_region(ctx)?;
    let s3 = Client::new(ctx, "s3")?;
    let file_path = a.str("file-path").unwrap_or_default();
    let bucket = a.str("bucket-name").unwrap_or_default();
    let remote = std::path::Path::new(&file_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let not_found = || ctx.general_error(&anyhow::anyhow!("{file_path} cannot be found"));
    let size = std::fs::metadata(&file_path)
        .map_err(|_| not_found())?
        .len();
    let body = std::fs::read(&file_path).map_err(|_| not_found())?;
    crate::session::useragent::register_command(&[crate::session::useragent::Feature::S3Transfer]);
    let result = s3.try_call(
        "PutObject",
        &map(vec![
            ("Bucket", Value::str(&bucket)),
            ("Key", Value::str(&remote)),
            ("Body", Value::Bytes(body)),
            ("ServerSideEncryption", Value::str("AES256")),
        ]),
    )?;
    if size > 0 {
        basic::eprint(&format!(
            "\rUploading to {remote}  {size} / {}  (100.00%)",
            crate::pyfmt::float_repr(size as f64)
        ));
    }
    match result {
        CallResult::Ok(_) => {}
        CallResult::ServiceError(info) if info.code == "NoSuchBucket" => {
            return Err(ctx.general_error(&anyhow::anyhow!(
                "S3 Bucket does not exist. Execute the command to create a new bucket\naws s3 mb s3://{bucket}"
            )));
        }
        CallResult::ServiceError(info) => return Err(client_error(ctx, &info)),
    }
    let base = if region != "us-east-1" {
        format!("https://s3-{region}.amazonaws.com")
    } else {
        "https://s3.amazonaws.com".to_string()
    };
    Ok(format!("{base}/{bucket}/{remote}"))
}

fn print_response(mut response: Value) {
    if let Some(m) = response.as_map_mut() {
        m.shift_remove("ResponseMetadata");
    }
    basic::print(&json_dumps_indent2(&response));
}

/// `GenerateProductCommand`.
fn product(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("product-name").required(),
        ArgDef::new("product-owner").required(),
        ArgDef::new("product-type")
            .required()
            .choices(&["CLOUD_FORMATION_TEMPLATE", "MARKETPLACE"]),
        ArgDef::new("product-description"),
        ArgDef::new("product-distributor"),
        ArgDef::new("tags")
            .schema(json!({"type": "array", "items": {"type": "string"}}))
            .default(NsVal::List(Vec::new())),
        ArgDef::new("file-path").required(),
        ArgDef::new("bucket-name").required(),
        ArgDef::new("support-description"),
        ArgDef::new("support-email"),
        ArgDef::new("provisioning-artifact-name").required(),
        ArgDef::new("provisioning-artifact-description").required(),
        artifact_type_arg(),
    ];
    let Some(a) = basic::parse(ctx, &["servicecatalog", "generate", "product"], &defs, args)?
    else {
        return Ok(ExitCode::Success);
    };
    let url = upload_template(ctx, &a)?;
    validate_region(ctx)?;
    let client = Client::new(ctx, "servicecatalog")?;
    let mut tags = Vec::new();
    for tag in a.str_list("tags").unwrap_or_default() {
        let mut m = Map::new();
        for (i, t) in tag.split(',').enumerate() {
            let parts: Vec<&str> = t.split('=').collect();
            if parts.len() != 2 {
                return Err(ctx.general_error(&anyhow::anyhow!(
                    "dictionary update sequence element #{i} has length {}; 2 is required",
                    parts.len()
                )));
            }
            m.insert(parts[0].to_string(), Value::str(parts[1]));
        }
        tags.push(Value::Map(m));
    }
    let s = |k: &str| Value::str(a.str(k).unwrap_or_default());
    let mut params = vec![
        ("Name", s("product-name")),
        ("Owner", s("product-owner")),
        ("ProductType", s("product-type")),
        ("Tags", Value::List(tags)),
        (
            "ProvisioningArtifactParameters",
            map(vec![
                ("Name", s("provisioning-artifact-name")),
                ("Description", s("provisioning-artifact-description")),
                ("Info", map(vec![("LoadTemplateFromURL", Value::str(&url))])),
                ("Type", s("provisioning-artifact-type")),
            ]),
        ),
    ];
    for (arg, key) in [
        ("support-description", "SupportDescription"),
        ("product-description", "Description"),
        ("support-email", "SupportEmail"),
        ("product-distributor", "Distributor"),
    ] {
        if a.str(arg).is_some_and(|v| !v.is_empty()) {
            params.push((key, s(arg)));
        }
    }
    let response = client.call("CreateProduct", &map(params))?;
    print_response(response);
    Ok(ExitCode::Success)
}

/// `GenerateProvisioningArtifactCommand`.
fn provisioning_artifact(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("file-path").required(),
        ArgDef::new("bucket-name").required(),
        ArgDef::new("provisioning-artifact-name").required(),
        ArgDef::new("provisioning-artifact-description").required(),
        artifact_type_arg(),
        ArgDef::new("product-id").required(),
    ];
    let Some(a) = basic::parse(
        ctx,
        &["servicecatalog", "generate", "provisioning-artifact"],
        &defs,
        args,
    )?
    else {
        return Ok(ExitCode::Success);
    };
    let url = upload_template(ctx, &a)?;
    validate_region(ctx)?;
    let client = Client::new(ctx, "servicecatalog")?;
    let s = |k: &str| Value::str(a.str(k).unwrap_or_default());
    let response = client.call(
        "CreateProvisioningArtifact",
        &map(vec![
            ("ProductId", s("product-id")),
            (
                "Parameters",
                map(vec![
                    ("Name", s("provisioning-artifact-name")),
                    ("Description", s("provisioning-artifact-description")),
                    ("Info", map(vec![("LoadTemplateFromURL", Value::str(&url))])),
                    ("Type", s("provisioning-artifact-type")),
                ]),
            ),
        ]),
    )?;
    print_response(response);
    Ok(ExitCode::Success)
}
