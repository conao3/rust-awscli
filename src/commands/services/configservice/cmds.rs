//! `configservice subscribe` and `configservice get-status`
//! (`awscli/customizations/configservice/subscribe.py`, `getstatus.py`).

use crate::commands::services::basic::{self, ArgDef};
use crate::commands::services::exec::{Client, sub_context};
use crate::driver::{CallResult, Context, client_error};
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "subscribe" => subscribe(ctx, args),
        "get-status" => get_status(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Map(m)
}

fn key_error(ctx: &Context, key: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("'{key}'"))
}

/// `json.dumps(value, indent=4)` (`ensure_ascii=True`).
pub(crate) fn json_dumps_indent4(value: &Value) -> String {
    let text = crate::pyfmt::json_dumps_pretty(value);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// `SubscribeCommand`.
fn subscribe(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("s3-bucket").required(),
        ArgDef::new("sns-topic").required(),
        ArgDef::new("iam-role").required(),
    ];
    let Some(a) = basic::parse(ctx, &["configservice", "subscribe"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let other_ctx = sub_context(ctx, None, None, None)?;
    let s3 = Client::new(&other_ctx, "s3")?;
    let sns = Client::new(&other_ctx, "sns")?;
    let config = Client::new(ctx, "config")?;

    let s3_path = a.str("s3-bucket").unwrap_or_default();
    let (bucket, prefix) = match s3_path.split_once('/') {
        Some((b, k)) => (b.to_string(), k.to_string()),
        None => (s3_path.clone(), String::new()),
    };
    let exists = match s3.try_call("HeadBucket", &map(vec![("Bucket", Value::str(&bucket))]))? {
        CallResult::Ok(_) => true,
        CallResult::ServiceError(info) => match info.code.trim().parse::<i64>() {
            Ok(404) => false,
            Ok(_) => true,
            Err(_) => {
                return Err(ctx.general_error(&anyhow::anyhow!(
                    "invalid literal for int() with base 10: {}",
                    crate::pyfmt::str_repr(&info.code)
                )));
            }
        },
    };
    if !exists {
        let region = ctx
            .globals
            .region
            .clone()
            .or_else(|| other_ctx.session.region())
            .unwrap_or_else(|| "us-east-1".to_string());
        let mut params = vec![("Bucket", Value::str(&bucket))];
        if region != "us-east-1" {
            params.push((
                "CreateBucketConfiguration",
                map(vec![("LocationConstraint", Value::str(&region))]),
            ));
        }
        s3.call("CreateBucket", &map(params))?;
        basic::print(&format!("Using new S3 bucket: {bucket}\n"));
    } else {
        basic::print(&format!("Using existing S3 bucket: {bucket}\n"));
    }

    let topic = a.str("sns-topic").unwrap_or_default();
    let topic_arn = if topic.contains(':') {
        basic::print(&format!("Using existing SNS topic: {topic}\n"));
        topic
    } else {
        let r = sns.call("CreateTopic", &map(vec![("Name", Value::str(&topic))]))?;
        let arn = r
            .get("TopicArn")
            .map(crate::pyfmt::py_str)
            .ok_or_else(|| key_error(ctx, "TopicArn"))?;
        basic::print(&format!("Using new SNS topic: {arn}\n"));
        arn
    };

    config.call(
        "PutConfigurationRecorder",
        &map(vec![(
            "ConfigurationRecorder",
            map(vec![
                ("name", Value::str("default")),
                ("roleARN", Value::str(a.str("iam-role").unwrap_or_default())),
            ]),
        )]),
    )?;
    let mut channel = vec![
        ("name", Value::str("default")),
        ("s3BucketName", Value::str(&bucket)),
        ("snsTopicARN", Value::str(&topic_arn)),
    ];
    if !prefix.is_empty() {
        channel.push(("s3KeyPrefix", Value::str(&prefix)));
    }
    config.call(
        "PutDeliveryChannel",
        &map(vec![("DeliveryChannel", map(channel))]),
    )?;
    config.call(
        "StartConfigurationRecorder",
        &map(vec![("ConfigurationRecorderName", Value::str("default"))]),
    )?;
    basic::print("Subscribe succeeded:\n\n");
    basic::print("Configuration Recorders: ");
    let r = config.call("DescribeConfigurationRecorders", &Value::map())?;
    let recorders = r
        .get("ConfigurationRecorders")
        .ok_or_else(|| key_error(ctx, "ConfigurationRecorders"))?;
    basic::print(&json_dumps_indent4(recorders));
    basic::print("\n\n");
    basic::print("Delivery Channels: ");
    let r = config.call("DescribeDeliveryChannels", &Value::map())?;
    let channels = r
        .get("DeliveryChannels")
        .ok_or_else(|| key_error(ctx, "DeliveryChannels"))?;
    basic::print(&json_dumps_indent4(channels));
    basic::print("\n");
    Ok(ExitCode::Success)
}

/// `GetStatusCommand`.
fn get_status(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    if basic::parse(ctx, &["configservice", "get-status"], &[], args)?.is_none() {
        return Ok(ExitCode::Success);
    }
    let config = Client::new(ctx, "config")?;
    let mut out = String::new();
    let flush = |out: &mut String| {
        basic::print(out);
        out.clear();
    };
    let get = |v: &Value, k: &str| -> Result<Value, CliError> {
        v.get(k).cloned().ok_or_else(|| key_error(ctx, k))
    };
    let last_status = |out: &mut String, status: &Value, name: &str| -> Result<(), CliError> {
        let last = get(status, "lastStatus")?;
        out.push_str(&format!(
            "last {name}status: {}\n",
            crate::pyfmt::py_str(&last)
        ));
        if last.as_str() == Some("FAILURE") {
            out.push_str(&format!(
                "error code: {}\n",
                crate::pyfmt::py_str(&get(status, "lastErrorCode")?)
            ));
            out.push_str(&format!(
                "message: {}\n",
                crate::pyfmt::py_str(&get(status, "lastErrorMessage")?)
            ));
        }
        Ok(())
    };
    let call = |op: &str| -> Result<Value, CliError> {
        match config.try_call(op, &Value::map())? {
            CallResult::Ok(v) => Ok(v),
            CallResult::ServiceError(info) => Err(client_error(ctx, &info)),
        }
    };
    let status = call("DescribeConfigurationRecorderStatus")?;
    out.push_str("Configuration Recorders:\n\n");
    flush(&mut out);
    let recorders = get(&status, "ConfigurationRecordersStatus")?;
    for r in recorders.as_list().cloned().unwrap_or_default() {
        let step = |out: &mut String| -> Result<(), CliError> {
            out.push_str(&format!(
                "name: {}\n",
                crate::pyfmt::py_str(&get(&r, "name")?)
            ));
            let recording = get(&r, "recording")?;
            let shown = match recording {
                Value::Bool(true) => "ON",
                Value::Bool(false) => "OFF",
                ref other => return Err(key_error(ctx, &crate::pyfmt::py_str(other))),
            };
            out.push_str(&format!("recorder: {shown}\n"));
            if recording.is_truthy() {
                last_status(out, &r, "")?;
            }
            out.push('\n');
            Ok(())
        };
        if let Err(e) = step(&mut out) {
            flush(&mut out);
            return Err(e);
        }
    }
    flush(&mut out);
    let status = call("DescribeDeliveryChannelStatus")?;
    out.push_str("Delivery Channels:\n\n");
    flush(&mut out);
    let channels = get(&status, "DeliveryChannelsStatus")?;
    for c in channels.as_list().cloned().unwrap_or_default() {
        let step = |out: &mut String| -> Result<(), CliError> {
            out.push_str(&format!(
                "name: {}\n",
                crate::pyfmt::py_str(&get(&c, "name")?)
            ));
            let stream = get(&c, "configStreamDeliveryInfo")?;
            let history = get(&c, "configHistoryDeliveryInfo")?;
            let snapshot = get(&c, "configSnapshotDeliveryInfo")?;
            for (info, name) in [
                (stream, "stream delivery "),
                (history, "history delivery "),
                (snapshot, "snapshot delivery "),
            ] {
                if info.is_truthy() {
                    last_status(out, &info, name)?;
                }
            }
            out.push('\n');
            Ok(())
        };
        if let Err(e) = step(&mut out) {
            flush(&mut out);
            return Err(e);
        }
    }
    flush(&mut out);
    Ok(ExitCode::Success)
}
