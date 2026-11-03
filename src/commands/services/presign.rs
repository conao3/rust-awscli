//! Presigned URLs (`botocore/auth.py` `SigV4QueryAuth`,
//! `botocore/signers.py` `RequestSigner.generate_presigned_url`) and the
//! commands built on them: `rds generate-db-auth-token`
//! (`awscli/customizations/rds.py`, `signers.generate_db_auth_token`).

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use super::basic::{self, ArgDef};
use crate::driver::{Context, block_on};
use crate::error::{CliError, ExitCode};
use crate::session::SessionError;
use crate::session::sign::{Credentials, SigV4, normalize_url_path};
use crate::session::url::{UrlParts, percent_encode_sequence, quote, urlsplit};

/// The headers `SigV4Auth.headers_to_sign` leaves out.
const BLACKLIST: &[&str] = &[
    "expect",
    "transfer-encoding",
    "user-agent",
    "x-amzn-trace-id",
];

/// A request to presign (`AWSRequest` after `prepare_request_dict`).
pub(crate) struct PresignRequest<'a> {
    pub method: &'a str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// The body parameters moved into the query string.
    pub body: Vec<(String, String)>,
}

/// `SigV4QueryAuth.add_auth` + `request.prepare()`: the presigned URL.
pub(crate) fn presign_url(
    req: &PresignRequest<'_>,
    credentials: &Credentials,
    service: &str,
    region: &str,
    expires: i64,
    now: DateTime<Utc>,
) -> String {
    let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let parts = urlsplit(&req.url);
    let host = UrlParts::parse(&req.url)
        .map(|p| p.host_header())
        .unwrap_or_default();
    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .filter(|(k, _)| !BLACKLIST.contains(&k.as_str()))
        .collect();
    if !headers.iter().any(|(k, _)| k == "host") {
        headers.push(("host".to_string(), host));
    }
    let mut names: Vec<String> = headers.iter().map(|(k, _)| k.clone()).collect();
    names.sort();
    names.dedup();
    let signed_headers = names.join(";");
    let scope = format!("{}/{region}/{service}/aws4_request", &timestamp[..8]);

    let mut query: Vec<(String, String)> = Vec::new();
    if !parts.query.is_empty() {
        for pair in parts.query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let k = percent_decode(k);
            let v = percent_decode(v);
            if let Some(e) = query.iter_mut().find(|(ek, _)| *ek == k) {
                e.1 = v;
            } else {
                query.push((k, v));
            }
        }
    }
    for (k, v) in &req.body {
        if let Some(e) = query.iter_mut().find(|(ek, _)| ek == k) {
            e.1 = v.clone();
        } else {
            query.push((k.clone(), v.clone()));
        }
    }
    let mut auth = vec![
        (
            "X-Amz-Algorithm".to_string(),
            "AWS4-HMAC-SHA256".to_string(),
        ),
        (
            "X-Amz-Credential".to_string(),
            format!("{}/{scope}", credentials.access_key),
        ),
        ("X-Amz-Date".to_string(), timestamp.clone()),
        ("X-Amz-Expires".to_string(), expires.to_string()),
        ("X-Amz-SignedHeaders".to_string(), signed_headers.clone()),
    ];
    if let Some(token) = &credentials.token {
        auth.push(("X-Amz-Security-Token".to_string(), token.clone()));
    }
    let mut new_query = String::new();
    if !query.is_empty() {
        new_query = percent_encode_sequence(&query) + "&";
    }
    new_query.push_str(&percent_encode_sequence(&auth));
    let mut url = format!("{}://{}{}", parts.scheme, parts.netloc, parts.path);
    url.push('?');
    url.push_str(&new_query);
    if !parts.fragment.is_empty() {
        url.push('#');
        url.push_str(&parts.fragment);
    }

    let mut pairs: Vec<(&str, &str)> = new_query
        .split('&')
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .collect();
    pairs.sort();
    let canonical_query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical_headers = names
        .iter()
        .map(|name| {
            let value = headers
                .iter()
                .filter(|(k, _)| k == name)
                .map(|(_, v)| v.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
                .join(",");
            format!("{name}:{value}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let path = quote(&normalize_url_path(&parts.path), "/~");
    let payload = hex::encode(Sha256::digest(b""));
    let canonical = format!(
        "{}\n{path}\n{canonical_query}\n{canonical_headers}\n\n{signed_headers}\n{payload}",
        req.method.to_uppercase()
    );
    let signer = SigV4 {
        credentials: credentials.clone(),
        service_name: service.to_string(),
        region_name: region.to_string(),
        s3: false,
    };
    let sts = signer.string_to_sign(&timestamp, &canonical);
    let signature = signer.signature(&timestamp, &sts);
    format!("{url}&X-Amz-Signature={signature}")
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(&s.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
}

/// The session's credentials, or `NoCredentialsError`.
pub(crate) fn credentials(ctx: &Context, region: Option<&str>) -> Result<Credentials, CliError> {
    match block_on(&ctx.runtime, ctx.session.credentials(region)) {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Err(ctx.session_error(&SessionError::no_credentials())),
        Err(e) => Err(ctx.session_error(&e)),
    }
}

/// The client region (`create_client(service, parsed_globals.region)`).
pub(crate) fn region(ctx: &Context) -> Result<Option<String>, CliError> {
    ctx.session
        .client_settings()
        .map(|s| s.region)
        .map_err(|e| ctx.session_error(&e))
}

/// `GenerateDBAuthTokenCommand._run_main` (the `rds` module routes
/// `generate-db-auth-token` here).
#[allow(dead_code)]
pub(crate) fn generate_db_auth_token(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = [
        ArgDef::new("hostname").required(),
        ArgDef::new("port").int().required(),
        ArgDef::new("username").required(),
    ];
    let Some(a) = basic::parse(ctx, &["rds", "generate-db-auth-token"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let region = region(ctx)?;
    let creds = credentials(ctx, region.as_deref())?;
    let hostname = a.str("hostname").unwrap_or_default();
    let port = a.str("port").unwrap_or_default();
    let req = PresignRequest {
        method: "GET",
        url: format!("https://{hostname}:{port}/"),
        headers: vec![],
        body: vec![
            ("Action".to_string(), "connect".to_string()),
            ("DBUser".to_string(), a.str("username").unwrap_or_default()),
        ],
    };
    let url = presign_url(
        &req,
        &creds,
        "rds-db",
        region.as_deref().unwrap_or_default(),
        900,
        Utc::now(),
    );
    basic::print(&format!("{}\n", &url["https://".len()..]));
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn creds(token: Option<&str>) -> Credentials {
        Credentials {
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            token: token.map(str::to_string),
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
    }

    /// Goldens from botocore with `datetime.datetime.utcnow` fixed to
    /// 2026-01-02T03:04:05 (`rds.generate_db_auth_token`,
    /// `dsql.generate_db_connect_auth_token`).
    #[test]
    fn rds_token_matches_botocore() {
        let req = PresignRequest {
            method: "GET",
            url: "https://db.example.com:5432/".to_string(),
            headers: vec![],
            body: vec![
                ("Action".to_string(), "connect".to_string()),
                ("DBUser".to_string(), "admin user".to_string()),
            ],
        };
        let url = presign_url(&req, &creds(None), "rds-db", "us-east-1", 900, now());
        assert_eq!(url, include_str!("testdata/rds_token.txt").trim_end());
        let url = presign_url(
            &req,
            &creds(Some("tok/en+=")),
            "rds-db",
            "us-west-2",
            900,
            now(),
        );
        assert_eq!(
            url,
            include_str!("testdata/rds_token_session.txt").trim_end()
        );
    }
}

/// The endpoint URL a client created without `endpoint_url` resolves for
/// `operation` (the configured endpoint URL, else the endpoint rule
/// set), or `endpoint_url` when given.
pub(crate) fn resolve_endpoint(
    ctx: &Context,
    service: &str,
    operation: &str,
    endpoint_url: Option<String>,
) -> Result<String, CliError> {
    use crate::endpoint::{self, BuiltinInputs, Builtins};
    let mut model = crate::model::ServiceModel::load(service).map_err(|e| ctx.general_error(&e))?;
    model.apply_endpoint_ruleset_customizations();
    let op = model
        .operation(operation)
        .ok_or_else(|| ctx.general_error(&anyhow::anyhow!("unknown operation {operation}")))?;
    let config = ctx.session.config();
    let inputs = BuiltinInputs {
        region_name: region(ctx)?,
        endpoint_prefix: model.endpoint_prefix().to_string(),
        use_fips_endpoint: config.get_bool("use_fips_endpoint").ok().flatten(),
        use_dualstack_endpoint: config.get_bool("use_dualstack_endpoint").ok().flatten(),
        endpoint_url: endpoint_url
            .or_else(|| config.configured_endpoint_url(&model.name).ok().flatten()),
        account_id_endpoint_mode: config.get("account_id_endpoint_mode").ok().flatten(),
        ..Default::default()
    };
    let builtins = Builtins::compute(&inputs);
    let ep =
        endpoint::construct_endpoint(&model, &op, &crate::value::Value::map(), &builtins, true)
            .map_err(|e| ctx.general_error(&anyhow::Error::new(e)))?;
    Ok(ep.url)
}
