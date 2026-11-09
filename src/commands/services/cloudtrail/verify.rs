//! `cloudtrail verify-query-results` (`verifyqueryresults.py`): the sign
//! file and export files of a CloudTrail Lake query, read from a local
//! directory or S3, checked against their hashes and the signature.

use super::*;

const SIGN_FILE_NAME: &str = "result_sign.json";

/// `format_prefix`.
fn format_prefix(prefix: &str) -> String {
    if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

/// `os.path.join(prefix, name)`.
fn join(prefix: &str, name: &str) -> String {
    if name.starts_with('/') || prefix.is_empty() || prefix.ends_with('/') {
        if name.starts_with('/') {
            name.to_string()
        } else {
            format!("{prefix}{name}")
        }
    } else {
        format!("{prefix}/{name}")
    }
}

/// Where the sign file and export files are read from.
enum Source<'c> {
    Local(String),
    S3 {
        client: S3Client<'c>,
        bucket: String,
        prefix: String,
    },
}

impl Source<'_> {
    fn read(&self, ctx: &Context, name: &str) -> Result<Vec<u8>, CliError> {
        match self {
            Source::Local(prefix) => {
                let path = join(prefix, name);
                std::fs::read(&path)
                    .map_err(|e| general(ctx, &crate::cli::paramfile::os_error(&e, &path)))
            }
            Source::S3 {
                client,
                bucket,
                prefix,
            } => {
                let mut p = Map::new();
                p.insert("Bucket".into(), Value::str(bucket));
                p.insert("Key".into(), Value::str(format!("{prefix}{name}")));
                let response = client.call("GetObject", p).map_err(|e| match e {
                    CallError::Service(info) => client_error(ctx, &info),
                    other => general(ctx, &other.to_string()),
                })?;
                Ok(match response.get("Body") {
                    Some(Value::Bytes(b)) => b.clone(),
                    Some(Value::Str(s)) => s.clone().into_bytes(),
                    _ => Vec::new(),
                })
            }
        }
    }
}

fn validation_error(ctx: &Context, message: &str) -> CliError {
    general(ctx, &format!("ValidationError: {message}"))
}

fn field<'v>(ctx: &Context, value: &'v Value, key: &str) -> Result<&'v Value, CliError> {
    match value {
        Value::Map(m) => m.get(key).ok_or_else(|| general(ctx, &format!("'{key}'"))),
        Value::List(_) => Err(general(
            ctx,
            "list indices must be integers or slices, not str",
        )),
        other => Err(general(
            ctx,
            &format!("'{}' object is not subscriptable", py_type_name(other)),
        )),
    }
}

fn text(ctx: &Context, value: &Value) -> Result<String, CliError> {
    crate::protocol::common::py_str(value).map_err(|e| general(ctx, &e.to_string()))
}

/// `CloudTrailVerifyQueryResult._run_main`.
pub(super) fn verify_query_results(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = [
        ArgDef::new("local-export-path").type_name("string"),
        ArgDef::new("s3-bucket").type_name("string"),
        ArgDef::new("s3-prefix").type_name("string"),
    ];
    let Some(a) = basic::parse(ctx, &["cloudtrail", "verify-query-results"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    // `handle_args`.
    let local = a.str("local-export-path").filter(|s| !s.is_empty());
    let bucket = a.str("s3-bucket").filter(|s| !s.is_empty());
    if local.is_none() && bucket.is_none() {
        return Err(basic::param_validation(
            ctx,
            "Require parameter --s3-bucket or --local-export-path.",
        ));
    }
    if local.is_some() && (bucket.is_some() || a.str("s3-prefix").is_some_and(|p| !p.is_empty())) {
        return Err(basic::param_validation(
            ctx,
            "Parameter --local-export-path can not be specified with parameter --s3-bucket nor --s3-prefix.",
        ));
    }
    // `setup_services` / `_initialize_components`.
    let cloudtrail = Client::new(ctx, "cloudtrail")?;
    let source = match local {
        Some(path) => Source::Local(path),
        None => Source::S3 {
            client: S3Client::with_config(
                ctx,
                ClientConfig {
                    region: ctx.globals.region.clone().map(Some),
                    without_endpoint_url: true,
                    id: 1,
                    ..Default::default()
                },
            )
            .map_err(|e| ctx.general_error(&e))?,
            bucket: bucket.unwrap_or_default(),
            prefix: format_prefix(&a.str("s3-prefix").unwrap_or_default()),
        },
    };
    // `provide_sign_file`.
    let raw = source.read(ctx, SIGN_FILE_NAME)?;
    let raw = String::from_utf8(raw).map_err(|e| {
        general(
            ctx,
            &format!(
                "'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte",
                e.as_bytes()[e.utf8_error().valid_up_to()],
                e.utf8_error().valid_up_to()
            ),
        )
    })?;
    let sign_file = crate::cli::pyjson::loads(&raw).map_err(|e| {
        general(
            ctx,
            &format!(
                "InformationCollectionError: Unable to load result_sign.json file, due to {e}"
            ),
        )
    })?;
    // `validate_export_files`.
    let files = field(ctx, &sign_file, "files")?;
    let files: Vec<Value> = match files {
        Value::List(l) => l.clone(),
        Value::Map(m) => m.keys().map(Value::str).collect(),
        Value::Str(s) => s.chars().map(|c| Value::str(c.to_string())).collect(),
        _ => return Err(validation_error(ctx, "Invalid sign file provided.")),
    };
    if files.is_empty() {
        return Err(validation_error(
            ctx,
            "No export file was found in sign file.",
        ));
    }
    for file in &files {
        let name = text(ctx, field(ctx, file, "fileName")?)?;
        let data = source.read(ctx, &name)?;
        let expected = field(ctx, file, "fileHashValue")?;
        let computed = hex::encode(Sha256::digest(&data));
        if Value::str(&computed) != *expected {
            return Err(validation_error(
                ctx,
                &format!(
                    "File {name} has inconsistent hash value with hash value recorded in sign file, hash value in sign file is {} , but get {computed}",
                    text(ctx, expected)?
                ),
            ));
        }
    }
    // `PublicKeyProvider.get_public_key`.
    let complete = PyDate::parse(field(ctx, &sign_file, "queryCompleteTime")?)
        .map_err(|e| general(ctx, &e))?;
    let fingerprint = field(ctx, &sign_file, "publicKeyFingerprint")?.clone();
    let search_end = PyDate {
        naive: complete.naive + Duration::days(20),
        offset: Some(0),
    }
    .compact();
    let mut p = Map::new();
    p.insert("StartTime".into(), complete.param());
    p.insert("EndTime".into(), Value::str(search_end));
    let keys = cloudtrail.call("ListPublicKeys", &Value::Map(p))?;
    let public_key = keys
        .get("PublicKeyList")
        .and_then(Value::as_list)
        .and_then(|l| {
            l.iter()
                .find(|k| k.get("Fingerprint") == Some(&fingerprint))
        })
        .and_then(|k| k.get("Value"))
        .cloned()
        .ok_or_else(|| {
            general(
                ctx,
                &format!(
                    "No public keys found for key with fingerprint: {}",
                    text(ctx, &fingerprint).unwrap_or_default()
                ),
            )
        })?;
    // `Sha256RsaSignatureValidator.validate`.
    let key_text = text(ctx, &public_key)?;
    let unable = || {
        validation_error(
            ctx,
            &format!("Sign file invalid, unable to load PKCS #1 key: {key_text}"),
        )
    };
    let der = py_b64decode(&key_text).map_err(|_| unable())?;
    // `RSA.new_public_key_from_der_data` raises a `RuntimeError` the
    // validator does not catch.
    let (n, e) = parse_rsa_public_key(&der).ok_or_else(|| {
        general(
            ctx,
            "7179 (AWS_ERROR_CAL_CRYPTO_OPERATION_FAILED): Unknown error when calling underlying Crypto library.",
        )
    })?;
    let signature = unhexlify(&text(ctx, field(ctx, &sign_file, "hashSignature")?)?)
        .map_err(|e| general(ctx, &e))?;
    let mut hashes = Vec::new();
    for file in &files {
        hashes.push(text(ctx, field(ctx, file, "fileHashValue")?)?);
    }
    if !rsa_verify(&n, &e, hashes.join(" ").as_bytes(), &signature) {
        return Err(validation_error(ctx, "Invalid signature in sign file"));
    }
    crate::output::stdout::write(b"Successfully validated sign and query result files\n");
    Ok(ExitCode::Success)
}
