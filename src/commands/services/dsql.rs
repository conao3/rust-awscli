//! `dsql generate-db-connect-auth-token` and
//! `generate-db-connect-admin-auth-token` (`awscli/customizations/dsql.py`,
//! `botocore/signers.py` `_dsql_generate_db_auth_token`).

use chrono::Utc;

use super::basic::{self, ArgDef};
use super::presign::{PresignRequest, credentials, presign_url, region};
use crate::cli::argparse::NsVal;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    let action = match name {
        "generate-db-connect-auth-token" => "DbConnect",
        "generate-db-connect-admin-auth-token" => "DbConnectAdmin",
        _ => return Err(crate::commands::not_implemented()),
    };
    let defs = [
        ArgDef::new("hostname").required(),
        ArgDef::new("expires-in")
            .int()
            .default(NsVal::Int(900.into())),
    ];
    let Some(a) = basic::parse(ctx, &["dsql", name], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let region = region(ctx)?;
    let creds = credentials(ctx, region.as_deref())?;
    let req = PresignRequest {
        method: "GET",
        url: format!("https://{}/", a.str("hostname").unwrap_or_default()),
        headers: vec![],
        body: vec![("Action".to_string(), action.to_string())],
    };
    let url = presign_url(
        &req,
        &creds,
        "dsql",
        region.as_deref().unwrap_or_default(),
        a.i64("expires-in").unwrap_or(900),
        Utc::now(),
    );
    basic::print(&format!("{}\n", &url["https://".len()..]));
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::sign::Credentials;
    use chrono::TimeZone;

    #[test]
    fn tokens_match_botocore() {
        let creds = Credentials {
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            token: None,
        };
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        for (action, expires, golden) in [
            ("DbConnect", 900, include_str!("testdata/dsql_token.txt")),
            (
                "DbConnectAdmin",
                3600,
                include_str!("testdata/dsql_admin_token.txt"),
            ),
        ] {
            let req = PresignRequest {
                method: "GET",
                url: "https://abc.dsql.us-east-1.on.aws/".to_string(),
                headers: vec![],
                body: vec![("Action".to_string(), action.to_string())],
            };
            let url = presign_url(&req, &creds, "dsql", "us-east-1", expires, now);
            assert_eq!(url, golden.trim_end());
        }
    }
}
