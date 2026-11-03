//! `cloudtrail validate-logs` (`awscli/customizations/cloudtrail/validation.py`):
//! the arguments, the time range, the trail ARN, and the trail lookup.
//! Walking the digest chain in S3 is reported as not implemented.

use chrono::{NaiveDate, NaiveDateTime, Utc};

use super::basic::{self, ArgDef};
use super::exec::Client;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::value::Value;

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "validate-logs" => validate_logs(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

fn general(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

/// `parse_date` (`dateutil.parser.parse`) for the ISO 8601 forms, then
/// `normalize_date` (the time zone is replaced, not converted).
fn parse_date(s: &str) -> Option<NaiveDateTime> {
    let t = s.trim();
    let (body, _) = split_offset(t);
    for f in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y%m%dT%H%M%S",
    ] {
        if let Ok(d) = NaiveDateTime::parse_from_str(body, f) {
            return Some(d);
        }
    }
    for f in ["%Y-%m-%d", "%Y%m%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(body, f) {
            return d.and_hms_opt(0, 0, 0);
        }
    }
    None
}

/// Strip a trailing `Z` or `±HH:MM` / `±HHMM` offset.
fn split_offset(s: &str) -> (&str, Option<&str>) {
    if let Some(b) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return (b, Some("Z"));
    }
    if s.len() > 10
        && let Some(i) = s[10..].rfind(['+', '-']).map(|i| i + 10)
    {
        let off = &s[i + 1..];
        if off.len() >= 2 && off.chars().all(|c| c.is_ascii_digit() || c == ':') {
            return (&s[..i], Some(&s[i..]));
        }
    }
    (s, None)
}

/// `CloudTrailValidateLogs._run_main` up to the digest traversal.
fn validate_logs(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = [
        ArgDef::new("trail-arn").type_name("string").required(),
        ArgDef::new("start-time").type_name("string").required(),
        ArgDef::new("end-time").type_name("string"),
        ArgDef::new("s3-bucket").type_name("string"),
        ArgDef::new("s3-prefix").type_name("string"),
        ArgDef::new("account-id").type_name("string"),
        ArgDef::new("verbose").type_name("boolean").flag(),
    ];
    let Some(a) = basic::parse(ctx, &["cloudtrail", "validate-logs"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let start_raw = a.str("start-time").unwrap_or_default();
    let start = parse_date(&start_raw)
        .ok_or_else(|| general(ctx, &format!("Unable to parse date value: {start_raw}")))?;
    let end = match a.str("end-time").filter(|s| !s.is_empty()) {
        Some(e) => parse_date(&e)
            .ok_or_else(|| general(ctx, &format!("Unable to parse date value: {e}")))?,
        None => Utc::now().naive_utc(),
    };
    if start > end {
        return Err(general(
            ctx,
            "Invalid time range specified: start-time must occur before end-time",
        ));
    }
    let arn = a.str("trail-arn").unwrap_or_default();
    let re = regex::Regex::new(r"^arn:.+:cloudtrail:.+:\d{12}:trail/.+").expect("regex");
    if !re.is_match(&arn) {
        return Err(general(ctx, &format!("Invalid trail ARN provided: {arn}")));
    }
    if a.get("s3-bucket").is_none() {
        let client = Client::new(ctx, "cloudtrail")?;
        let trails = client.call("DescribeTrails", &Value::map())?;
        let trail = trails
            .get("trailList")
            .and_then(Value::as_list)
            .and_then(|l| {
                l.iter()
                    .find(|t| t.get("TrailARN").and_then(Value::as_str) == Some(arn.as_str()))
            })
            .cloned()
            .ok_or_else(|| general(ctx, &format!("A trail could not be found for {arn}")))?;
        if trail.get("S3BucketName").is_none() {
            return Err(general(ctx, "'S3BucketName'"));
        }
        if trail
            .get("IsOrganizationTrail")
            .is_some_and(Value::is_truthy)
            && !a.get("account-id").is_some_and(Value::is_truthy)
        {
            return Err(general(
                ctx,
                "Missing required parameter for organization trail: '--account-id'",
            ));
        }
    }
    Err(crate::commands::not_implemented())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        let d = |s| parse_date(s).map(|d| d.to_string());
        assert_eq!(
            d("2015-01-08T05:21:42Z").as_deref(),
            Some("2015-01-08 05:21:42")
        );
        assert_eq!(d("2015-01-08").as_deref(), Some("2015-01-08 00:00:00"));
        assert_eq!(
            d("2015-01-08T05:21:42+09:00").as_deref(),
            Some("2015-01-08 05:21:42")
        );
        assert_eq!(d("nope"), None);
    }
}
