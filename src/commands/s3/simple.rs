//! `aws s3 ls`, `mb`, `rb`, `website`, and `presign`
//! (`awscli/customizations/s3/subcommands.py` `ListCommand`,
//! `MbCommand`, `RbCommand`, `WebsiteCommand`, `PresignCommand`).

use std::io::Write;

use super::args::{Args, tag_list};
use super::client::{CallError, Client, map};
use super::utils::{
    block_unsupported_resources, find_bucket_key, human_readable_size, is_s3express_bucket,
    local_time, parse_timestamp, split_s3_bucket_key,
};
use super::{S3Fail, out};
use crate::cli::Fail;
use crate::driver::{Context, block_on};
use crate::endpoint::{self, BuiltinInputs, Builtins, S3Config};
use crate::error::ExitCode;
use crate::session::sign::{Credentials, SigV4};
use crate::session::url::{UrlParts, percent_encode_sequence, urlsplit};
use crate::value::{Map, Value};

/// `ListCommand._make_last_mod_str`.
pub(crate) fn last_mod_str(timestamp: &str) -> String {
    let secs = parse_timestamp(timestamp).map_or(0, |t| t.secs());
    let (y, mo, d, h, mi, s) = local_time(secs);
    format!("{y}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

struct Ls {
    empty_result: bool,
    at_first_page: bool,
    size_accumulator: i64,
    total_objects: i64,
    human_readable: bool,
}

impl Ls {
    fn size_str(&self, size: i64) -> String {
        let s = if self.human_readable {
            human_readable_size(size as f64)
        } else {
            size.to_string()
        };
        format!("{s:>10}")
    }

    /// `_display_page`.
    fn display_page(&mut self, page: &Map, use_basename: bool) {
        let empty = vec![];
        let prefixes = page
            .get("CommonPrefixes")
            .and_then(Value::as_list)
            .unwrap_or(&empty);
        let contents = page
            .get("Contents")
            .and_then(Value::as_list)
            .unwrap_or(&empty);
        if contents.is_empty() && prefixes.is_empty() {
            self.empty_result = true;
            return;
        }
        let mut buf = String::new();
        for p in prefixes {
            let prefix = p.get("Prefix").and_then(Value::as_str).unwrap_or_default();
            let comps: Vec<&str> = prefix.split('/').collect();
            let name = if comps.len() >= 2 {
                comps[comps.len() - 2]
            } else {
                ""
            };
            buf.push_str(&format!("{:>30} {name}/\n", "PRE"));
        }
        for c in contents {
            let last_mod = last_mod_str(
                c.get("LastModified")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            let size = c.get("Size").and_then(Value::as_i64).unwrap_or(0);
            self.size_accumulator += size;
            self.total_objects += 1;
            let key = c.get("Key").and_then(Value::as_str).unwrap_or_default();
            let name = if use_basename {
                key.rsplit('/').next().unwrap_or(key)
            } else {
                key
            };
            buf.push_str(&format!("{last_mod:<19} {} {name}\n", self.size_str(size)));
        }
        out(buf.as_bytes());
        self.at_first_page = false;
    }
}

fn service(e: CallError) -> S3Fail {
    S3Fail::Call(e)
}

/// `ListCommand._run_main`.
pub(crate) fn ls(ctx: &Context, args: &Args) -> Result<ExitCode, S3Fail> {
    let client = Client::new(ctx).map_err(S3Fail::Other)?;
    let mut st = Ls {
        empty_result: false,
        at_first_page: true,
        size_accumulator: 0,
        total_objects: 0,
        human_readable: args.truthy("human_readable"),
    };
    let path = args.str("paths").unwrap_or("s3://");
    let path = path.strip_prefix("s3://").unwrap_or(path);
    let (bucket, key) = find_bucket_key(path)?;
    let page_size = args.int("page_size");
    if bucket.is_empty() {
        let mut params = Map::new();
        if let Some(p) = args.str("bucket_name_prefix").filter(|p| !p.is_empty()) {
            params.insert("Prefix".into(), Value::str(p));
        }
        if let Some(r) = args.str("bucket_region").filter(|r| !r.is_empty()) {
            params.insert("BucketRegion".into(), Value::str(r));
        }
        if let Some(n) = page_size {
            params.insert("MaxBuckets".into(), Value::int(n));
        }
        loop {
            let page = client
                .call("ListBuckets", params.clone())
                .map_err(service)?;
            let mut buf = String::new();
            for b in page
                .get("Buckets")
                .and_then(Value::as_list)
                .cloned()
                .unwrap_or_default()
            {
                let date = last_mod_str(
                    b.get("CreationDate")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                let name = b.get("Name").and_then(Value::as_str).unwrap_or_default();
                buf.push_str(&format!("{date:<19} {name}\n"));
            }
            out(buf.as_bytes());
            match page.get("ContinuationToken").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => {
                    params.insert("ContinuationToken".into(), Value::str(t));
                }
                _ => break,
            }
        }
    } else {
        let recursive = args.truthy("dir_op");
        let mut params = map(&[
            ("Bucket", Value::str(&bucket)),
            ("Prefix", Value::str(&key)),
        ]);
        if !recursive {
            params.insert("Delimiter".into(), Value::str("/"));
        }
        if let Some(rp) = args.get("request_payer") {
            params.insert("RequestPayer".into(), rp.clone());
        }
        client
            .list_objects_v2_pages(params, page_size, &mut |page| {
                st.display_page(&page, !recursive);
                Ok(true)
            })
            .map_err(service)?;
    }
    if args.truthy("summarize") {
        let total = if st.human_readable {
            human_readable_size(st.size_accumulator as f64)
        } else {
            st.size_accumulator.to_string()
        };
        out(format!(
            "\nTotal Objects: {}\n{:>15}{total}\n",
            st.total_objects, "Total Size: "
        )
        .as_bytes());
    }
    if !key.is_empty() && st.empty_result && st.at_first_page {
        return Ok(ExitCode::S3TransferFailed);
    }
    Ok(ExitCode::Success)
}

fn region(ctx: &Context) -> String {
    ctx.session
        .client_settings()
        .ok()
        .and_then(|s| s.region)
        .unwrap_or_else(|| "us-east-1".to_string())
}

fn err_line(text: &str) {
    let mut e = std::io::stderr().lock();
    let _ = e.write_all(text.as_bytes());
    let _ = e.flush();
}

/// `MbCommand._run_main`.
pub(crate) fn mb(ctx: &Context, args: &Args) -> Result<ExitCode, S3Fail> {
    let client = Client::new(ctx).map_err(S3Fail::Other)?;
    let path = args.str("path").unwrap_or_default();
    if !path.starts_with("s3://") {
        return Err(Fail::ParamValidation("<S3Uri>\nError: Invalid argument type".into()).into());
    }
    let (bucket, _) = split_s3_bucket_key(path)?;
    if is_s3express_bucket(&bucket) {
        return Err(
            Fail::ParamValidation("Cannot use mb command with a directory bucket.".into()).into(),
        );
    }
    let mut params = map(&[("Bucket", Value::str(&bucket))]);
    let mut config = Map::new();
    let tags = tag_list(args);
    if bucket.ends_with("-an") {
        params.insert("BucketNamespace".into(), Value::str("account-regional"));
    }
    let region = region(ctx);
    if region != "us-east-1" {
        config.insert("LocationConstraint".into(), Value::str(region));
    }
    if !tags.is_empty() {
        config.insert("Tags".into(), Value::List(tags));
    }
    if !config.is_empty() {
        params.insert("CreateBucketConfiguration".into(), Value::Map(config));
    }
    match client.call("CreateBucket", params) {
        Ok(_) => {
            out(format!("make_bucket: {bucket}\n").as_bytes());
            Ok(ExitCode::Success)
        }
        Err(e) => {
            err_line(&format!("make_bucket failed: {path} {e}\n"));
            Ok(ExitCode::S3TransferFailed)
        }
    }
}

/// `RbCommand._run_main`.
pub(crate) fn rb(ctx: &Context, args: &Args) -> Result<ExitCode, S3Fail> {
    let client = Client::new(ctx).map_err(S3Fail::Other)?;
    let path = args.str("path").unwrap_or_default().to_string();
    if !path.starts_with("s3://") {
        return Err(Fail::ParamValidation("<S3Uri>\nError: Invalid argument type".into()).into());
    }
    let (bucket, key) = split_s3_bucket_key(&path)?;
    if !key.is_empty() {
        return Err(Fail::ParamValidation(format!(
            "Please specify a valid bucket name only. E.g. s3://{bucket}"
        ))
        .into());
    }
    if args.truthy("force") {
        let rc = super::run_subcommand(ctx, "rm", &[path.clone(), "--recursive".to_string()])?;
        if rc != ExitCode::Success {
            return Err(S3Fail::Other(anyhow::anyhow!(
                "remove_bucket failed: Unable to delete all objects in the bucket, bucket will not be deleted."
            )));
        }
    }
    match client.call("DeleteBucket", map(&[("Bucket", Value::str(&bucket))])) {
        Ok(_) => {
            out(format!("remove_bucket: {bucket}\n").as_bytes());
            Ok(ExitCode::Success)
        }
        Err(e) => {
            err_line(&format!("remove_bucket failed: {path} {e}\n"));
            Ok(ExitCode::S3TransferFailed)
        }
    }
}

/// `WebsiteCommand._run_main`.
pub(crate) fn website(ctx: &Context, args: &Args) -> Result<ExitCode, S3Fail> {
    let client = Client::new(ctx).map_err(S3Fail::Other)?;
    let first = match args.get("paths") {
        Some(Value::List(l)) => l.first().and_then(Value::as_str).unwrap_or("").to_string(),
        Some(Value::Str(s)) => s.chars().next().map(String::from).unwrap_or_default(),
        _ => String::new(),
    };
    let mut path = first.strip_prefix("s3://").unwrap_or(&first).to_string();
    if path.ends_with('/') {
        path.pop();
    }
    block_unsupported_resources(&path)?;
    let mut config = Map::new();
    if let Some(i) = args.get("index_document") {
        config.insert(
            "IndexDocument".into(),
            Value::Map(map(&[("Suffix", i.clone())])),
        );
    }
    if let Some(e) = args.get("error_document") {
        config.insert(
            "ErrorDocument".into(),
            Value::Map(map(&[("Key", e.clone())])),
        );
    }
    client
        .call(
            "PutBucketWebsite",
            map(&[
                ("Bucket", Value::str(path)),
                ("WebsiteConfiguration", Value::Map(config)),
            ]),
        )
        .map_err(service)?;
    Ok(ExitCode::Success)
}

/// The builtins `Caller::new` computes for the S3 client.
fn s3_builtins(ctx: &Context, model: &crate::model::ServiceModel) -> anyhow::Result<Builtins> {
    let settings = ctx.session.client_settings()?;
    let config = ctx.session.config();
    let s3_get = |key: &str| config.get(&format!("s3.{key}")).ok().flatten();
    let s3_bool = |key: &str| config.get_bool(&format!("s3.{key}")).ok().flatten();
    let inputs = BuiltinInputs {
        region_name: settings.region.clone(),
        endpoint_prefix: model.endpoint_prefix().to_string(),
        s3_config: S3Config {
            addressing_style: s3_get("addressing_style"),
            use_accelerate_endpoint: s3_bool("use_accelerate_endpoint"),
            use_dualstack_endpoint: s3_bool("use_dualstack_endpoint"),
            payload_signing_enabled: s3_bool("payload_signing_enabled"),
            use_arn_region: config
                .get_bool("s3_use_arn_region")
                .ok()
                .flatten()
                .or_else(|| s3_bool("use_arn_region")),
            s3_disable_multiregion_access_points: config
                .get_bool("s3_disable_multiregion_access_points")
                .ok()
                .flatten()
                .or_else(|| s3_bool("s3_disable_multiregion_access_points")),
        },
        use_fips_endpoint: config.get_bool("use_fips_endpoint").ok().flatten(),
        use_dualstack_endpoint: config.get_bool("use_dualstack_endpoint").ok().flatten(),
        endpoint_url: ctx
            .globals
            .endpoint_url
            .clone()
            .or_else(|| config.configured_endpoint_url(&model.name).ok().flatten()),
        account_id: None,
        account_id_endpoint_mode: config.get("account_id_endpoint_mode").ok().flatten(),
        s3_disable_express_session_auth: config
            .get_bool("s3_disable_express_session_auth")
            .ok()
            .flatten(),
        client_context_params: Default::default(),
    };
    Ok(Builtins::compute(&inputs))
}

/// `S3SigV4QueryAuth` applied to `url` (a GET without other headers) at
/// `timestamp` (`%Y%m%dT%H%M%SZ`).
pub(crate) fn presign_url(
    url: &str,
    credentials: &Credentials,
    region: &str,
    signing_name: &str,
    expires: i64,
    timestamp: &str,
) -> String {
    let signer = SigV4 {
        credentials: credentials.clone(),
        service_name: signing_name.to_string(),
        region_name: region.to_string(),
        s3: true,
    };
    let parts = urlsplit(url);
    let host = UrlParts::parse(url)
        .map(|p| p.host_header())
        .unwrap_or_default();
    let scope = format!(
        "{}/{}/{region}/{signing_name}/aws4_request",
        credentials.access_key,
        &timestamp[..8]
    );
    let mut auth = vec![
        (
            "X-Amz-Algorithm".to_string(),
            "AWS4-HMAC-SHA256".to_string(),
        ),
        ("X-Amz-Credential".to_string(), scope),
        ("X-Amz-Date".to_string(), timestamp.to_string()),
        ("X-Amz-Expires".to_string(), expires.to_string()),
        ("X-Amz-SignedHeaders".to_string(), "host".to_string()),
    ];
    if let Some(t) = credentials.token.as_deref() {
        auth.push(("X-Amz-Security-Token".to_string(), t.to_string()));
    }
    let mut operation_params: Vec<(String, String)> = vec![];
    for p in parts.query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        let k = percent_decode(k);
        let v = percent_decode(v);
        match operation_params.iter_mut().find(|(n, _)| *n == k) {
            Some(_) => {}
            None => operation_params.push((k, v)),
        }
    }
    let mut query = String::new();
    if !operation_params.is_empty() {
        query.push_str(&percent_encode_sequence(&operation_params));
        query.push('&');
    }
    query.push_str(&percent_encode_sequence(&auth));
    let new_url = format!("{}://{}{}?{query}", parts.scheme, parts.netloc, parts.path);
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .collect();
    pairs.sort();
    let canonical_query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let path = if parts.path.is_empty() {
        "/".to_string()
    } else {
        parts.path.clone()
    };
    let canonical =
        format!("GET\n{path}\n{canonical_query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
    let sts = signer.string_to_sign(timestamp, &canonical);
    let signature = signer.signature(timestamp, &sts);
    format!("{new_url}&X-Amz-Signature={signature}")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `PresignCommand._run_main` (`generate_presigned_url('get_object', ...)`).
pub(crate) fn presign(ctx: &Context, args: &Args) -> Result<ExitCode, S3Fail> {
    let model = super::client::s3_model().map_err(S3Fail::Other)?;
    let path = args.str("path").unwrap_or_default();
    let path = path.strip_prefix("s3://").unwrap_or(path);
    let (bucket, key) = find_bucket_key(path)?;
    let expires = args.int("expires_in").unwrap_or(3600);
    let params = Value::Map(map(&[
        ("Bucket", Value::str(bucket)),
        ("Key", Value::str(key)),
    ]));
    let operation = model.operation("GetObject").expect("GetObject");
    if let Some(input) = operation.input()
        && let Some(msg) = crate::cli::validate::validate(&params, &input)
    {
        return Err(Fail::ParamValidation(msg).into());
    }
    let mut builtins = s3_builtins(ctx, model).map_err(S3Fail::Other)?;
    endpoint::customize_s3_builtins(&mut builtins, "GetObject", &params);
    let use_ssl = !ctx
        .globals
        .endpoint_url
        .as_deref()
        .is_some_and(|u| u.starts_with("http://"));
    let ep = endpoint::construct_endpoint(model, &operation, &params, &builtins, use_ssl)
        .map_err(|e| S3Fail::Other(e.into()))?;
    let request = crate::protocol::serialize(model, &operation, &params).map_err(S3Fail::Other)?;
    let url = ctx
        .session
        .request_url(&ep, &request)
        .map_err(|e| S3Fail::Other(e.into()))?;
    let scheme = ep
        .properties
        .get("authSchemes")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|a| a.as_object());
    let region_default = region(ctx);
    let signing_region = scheme
        .and_then(|s| s.get("signingRegion"))
        .and_then(|v| v.as_str())
        .unwrap_or(&region_default)
        .to_string();
    let signing_name = scheme
        .and_then(|s| s.get("signingName"))
        .and_then(|v| v.as_str())
        .unwrap_or("s3")
        .to_string();
    let credentials = block_on(
        &ctx.runtime,
        ctx.session.credentials(Some(signing_region.as_str())),
    )
    .map_err(|e| S3Fail::Other(e.into()))?
    .ok_or_else(|| S3Fail::Other(anyhow::anyhow!("Unable to locate credentials")))?;
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let url = presign_url(
        &url,
        &credentials,
        &signing_region,
        &signing_name,
        expires,
        &timestamp,
    );
    out(format!("{url}\n").as_bytes());
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presigned_url_matches_botocore() {
        // botocore S3SigV4QueryAuth with a fixed timestamp.
        let creds = Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "difftest-fake-secret-key".into(),
            token: None,
        };
        let url = presign_url(
            "https://src.s3.us-east-1.amazonaws.com/a%20b%2Bc~%25.txt",
            &creds,
            "us-east-1",
            "s3",
            60,
            "20261103T170606Z",
        );
        assert_eq!(
            url,
            "https://src.s3.us-east-1.amazonaws.com/a%20b%2Bc~%25.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIDEXAMPLE%2F20261103%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20261103T170606Z&X-Amz-Expires=60&X-Amz-SignedHeaders=host&X-Amz-Signature=67a4a828f2b3eb799f134bee98aa56169ff47ede4a458252963b6d0c8ead4116"
        );
    }
}
