//! `logs tail` (`awscli/customizations/logs/tail.py`) and
//! `logs start-live-tail` (`startlivetail.py`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regex::Regex;

use super::super::basic::{self, ArgDef, Args};
use super::super::exec::Client;
use crate::cli::argparse::Nargs;
use crate::cli::{Fail, pyjson, render_fail, validate};
use crate::driver::{CallResult, Caller, Context, client_error};
use crate::error::{CliError, ExitCode};
use crate::output::Color;
use crate::paginate::{PageIterator, PaginationConfig, limit_key_type};
use crate::protocol::timestamp;
use crate::value::{Map, Value};

const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const RESET_ALL: &str = "\x1b[0m";

fn tail_args() -> Vec<ArgDef> {
    vec![
        ArgDef::new("group_name").positional(),
        ArgDef::new("since").default_str("10m"),
        ArgDef::new("follow").flag(),
        ArgDef::new("format")
            .default_str("detailed")
            .choices(&["detailed", "short", "json"]),
        ArgDef::new("filter-pattern"),
        ArgDef::new("log-stream-names").nargs(Nargs::OneOrMore),
        ArgDef::new("log-stream-name-prefix"),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Detailed,
    Short,
    Json,
}

/// `TailCommand._run_main`.
pub(super) fn tail(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let Some(a) = basic::parse(ctx, &["logs", "tail"], &tail_args(), args)? else {
        return Ok(ExitCode::Success);
    };
    let client = Client::new(ctx, "logs")?;
    let params = filter_kwargs(ctx, &a)?;
    let format = match a.str("format").as_deref() {
        Some("short") => Format::Short,
        Some("json") => Format::Json,
        _ => Format::Detailed,
    };
    let colorize = match ctx.color() {
        Color::On => true,
        Color::Off => false,
        Color::Auto => ctx.terminal.stdout_is_tty,
    };
    let mut display = |event: &Value| -> Result<(), CliError> {
        let line = format_event(event, format, colorize)
            .map_err(|e| ctx.general_error(&anyhow::anyhow!("{e}")))?;
        basic::print(&line);
        Ok(())
    };
    if a.bool("follow") {
        follow(ctx, &client, params, &mut display)?;
    } else {
        no_follow(ctx, &client, &params, &mut display)?;
    }
    Ok(ExitCode::Success)
}

/// `BaseLogEventsGenerator._get_filter_logs_events_kwargs`.
fn filter_kwargs(ctx: &Context, a: &Args) -> Result<Map, CliError> {
    let mut kwargs = Map::new();
    kwargs.insert(
        "logGroupName".to_string(),
        a.get("group_name").cloned().unwrap_or(Value::Null),
    );
    kwargs.insert("interleaved".to_string(), Value::Bool(true));
    if let Some(since) = a.str("since") {
        let millis = to_epoch_millis(&since).map_err(|e| ctx.general_error(&e))?;
        kwargs.insert("startTime".to_string(), Value::int(millis));
    }
    for (arg, key) in [
        ("filter-pattern", "filterPattern"),
        ("log-stream-names", "logStreamNames"),
        ("log-stream-name-prefix", "logStreamNamePrefix"),
    ] {
        if let Some(v) = a.get(arg) {
            kwargs.insert(key.to_string(), v.clone());
        }
    }
    Ok(kwargs)
}

/// `TimestampUtils.to_epoch_millis`.
fn to_epoch_millis(since: &str) -> anyhow::Result<i64> {
    static RELATIVE: OnceLock<Regex> = OnceLock::new();
    let re = RELATIVE.get_or_init(|| Regex::new(r"\A(\d+)(s|m|h|d|w)\n?\z").expect("regex"));
    let seconds: f64 = if let Some(c) = re.captures(since) {
        let amount: i64 = c[1].parse()?;
        let multiplier = match &c[2] {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 24 * 3600,
            _ => 7 * 24 * 3600,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let now_us = now.as_secs() as i128 * 1_000_000 + i128::from(now.subsec_micros());
        let us = now_us - i128::from(amount) * i128::from(multiplier) * 1_000_000;
        us as f64 / 1e6
    } else {
        match timestamp::serialize(&Value::str(since), "unixtimestamp")? {
            Value::Int(i) => num_traits::ToPrimitive::to_f64(&i).unwrap_or_default(),
            Value::Float(f) => f,
            _ => 0.0,
        }
    };
    Ok((seconds * 1000.0) as i64)
}

/// `datetime.fromtimestamp(millis / 1000.0, tzutc())`: epoch seconds and
/// microseconds (with Python's round-half-even of the fraction).
fn from_millis(millis: &Value) -> Result<(i64, u32), String> {
    let t = match millis {
        Value::Int(i) => num_traits::ToPrimitive::to_f64(i).unwrap_or_default() / 1000.0,
        Value::Float(f) => f / 1000.0,
        Value::Bool(b) => f64::from(u8::from(*b)) / 1000.0,
        other => {
            return Err(format!(
                "unsupported operand type(s) for /: '{}' and 'float'",
                crate::cli::shorthand::py_type_name(other)
            ));
        }
    };
    let mut secs = t.trunc();
    let frac = t - secs;
    let mut us = (frac * 1e6).round_ties_even();
    if us >= 1_000_000.0 {
        secs += 1.0;
        us -= 1_000_000.0;
    } else if us < 0.0 {
        secs -= 1.0;
        us += 1_000_000.0;
    }
    Ok((secs as i64, us as u32))
}

/// Civil date and time of epoch seconds (UTC).
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

/// The timestamp text of each formatter.
fn format_timestamp(secs: i64, us: u32, format: Format) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    let base = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}");
    match format {
        Format::Short => base,
        Format::Detailed => format!("{base}.{us:06}+00:00"),
        Format::Json if us != 0 => format!("{base}.{us:06}+00:00"),
        Format::Json => format!("{base}+00:00"),
    }
}

fn color(text: &str, code: &str, colorize: bool) -> String {
    if colorize {
        format!("{code}{text}{RESET_ALL}")
    } else {
        text.to_string()
    }
}

/// Python `str.isspace` for one character.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn key<'a>(event: &'a Value, name: &str) -> Result<&'a Value, String> {
    event.get(name).ok_or_else(|| format!("'{name}'"))
}

fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => crate::pyfmt::py_str(other),
    }
}

/// `display_log_event` of the formatter (after `_convert_event_timestamps`).
fn format_event(event: &Value, format: Format, colorize: bool) -> Result<String, String> {
    from_millis(key(event, "ingestionTime")?)?;
    let (secs, us) = from_millis(key(event, "timestamp")?)?;
    let ts = color(&format_timestamp(secs, us, format), GREEN, colorize);
    let line = match format {
        Format::Short => format!("{ts} {}", text(key(event, "message")?)),
        Format::Detailed => format!(
            "{ts} {} {}",
            color(&text(key(event, "logStreamName")?), CYAN, colorize),
            text(key(event, "message")?)
        ),
        Format::Json => {
            let stream = color(&text(key(event, "logStreamName")?), CYAN, colorize);
            let message = text(key(event, "message")?);
            let message = match pyjson::loads(&message) {
                Ok(v) => format!("\n{}", crate::pyfmt::json_dumps_indent_ascii(&v)),
                Err(_) => message,
            };
            format!("{ts} {stream} {message}")
        }
    };
    Ok(format!("{}\n", line.trim_end_matches(is_py_space)))
}

fn validate_params(ctx: &Context, client: &Client<'_>, params: &Value) -> Result<(), CliError> {
    let op = client
        .model
        .operation("FilterLogEvents")
        .expect("FilterLogEvents");
    if let Some(input) = op.input()
        && let Some(report) = validate::validate(params, &input)
    {
        return Err(render_fail(
            Fail::ParamValidation(report),
            &ctx.globals,
            &ctx.session,
        ));
    }
    Ok(())
}

fn events(ctx: &Context, page: &Value) -> Result<Vec<Value>, CliError> {
    match page.get("events") {
        Some(Value::List(l)) => Ok(l.clone()),
        Some(_) => Ok(Vec::new()),
        None => Err(ctx.general_error(&anyhow::anyhow!("'events'"))),
    }
}

/// `NoFollowLogEventsGenerator`: every page of the `filter_log_events`
/// paginator.
fn no_follow(
    ctx: &Context,
    client: &Client<'_>,
    params: &Map,
    display: &mut dyn FnMut(&Value) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let params = Value::Map(params.clone());
    validate_params(ctx, client, &params)?;
    let op = client
        .model
        .operation("FilterLogEvents")
        .expect("FilterLogEvents");
    let paginator = client
        .model
        .paginator("FilterLogEvents")
        .expect("FilterLogEvents paginator");
    let caller =
        Caller::new(ctx, &client.model, "FilterLogEvents").map_err(|e| ctx.general_error(&e))?;
    let mut it = PageIterator::new(
        paginator,
        &params,
        &PaginationConfig::default(),
        limit_key_type(&op, paginator),
    )
    .map_err(|e| ctx.general_error(&e))?;
    let mut service_error = None;
    loop {
        let mut call = |p: &Value| -> anyhow::Result<Value> {
            match caller.call(p)? {
                CallResult::Ok(v) => Ok(v),
                CallResult::ServiceError(info) => {
                    service_error = Some(info);
                    Err(anyhow::anyhow!("service error"))
                }
            }
        };
        let page = match it.next_page(&mut call) {
            Ok(Some(page)) => page,
            Ok(None) => return Ok(()),
            Err(e) => {
                return Err(match service_error.take() {
                    Some(info) => client_error(ctx, &info),
                    None => ctx.general_error(&e),
                });
            }
        };
        for event in events(ctx, &page)? {
            display(&event)?;
        }
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// `FollowLogEventsGenerator`: poll until Control-C, skipping events
/// already shown for the newest timestamp.
fn follow(
    ctx: &Context,
    client: &Client<'_>,
    mut params: Map,
    display: &mut dyn FnMut(&Value) -> Result<(), CliError>,
) -> Result<(), CliError> {
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(
            libc::SIGINT,
            on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
    let mut seen: Vec<(Value, Vec<Value>)> = Vec::new();
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(());
        }
        let p = Value::Map(params.clone());
        let response = match client.try_call("FilterLogEvents", &p)? {
            CallResult::Ok(v) => v,
            CallResult::ServiceError(info) => {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    return Ok(());
                }
                return Err(client_error(ctx, &info));
            }
        };
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(());
        }
        for event in events(ctx, &response)? {
            let ts = event
                .get("timestamp")
                .cloned()
                .ok_or_else(|| ctx.general_error(&anyhow::anyhow!("'timestamp'")))?;
            let id = event
                .get("eventId")
                .cloned()
                .ok_or_else(|| ctx.general_error(&anyhow::anyhow!("'eventId'")))?;
            let entry = match seen.iter().position(|(t, _)| *t == ts) {
                Some(i) => &mut seen[i].1,
                None => {
                    seen.push((ts.clone(), Vec::new()));
                    &mut seen.last_mut().expect("pushed").1
                }
            };
            if !entry.contains(&id) {
                entry.push(id);
                display(&event)?;
            }
        }
        let newest = seen
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .map(|(t, _)| t.clone())
            .max_by(|a, b| {
                let f = |v: &Value| match v {
                    Value::Int(i) => num_traits::ToPrimitive::to_f64(i).unwrap_or_default(),
                    Value::Float(f) => *f,
                    _ => 0.0,
                };
                f(a).total_cmp(&f(b))
            });
        if let Some(newest) = &newest {
            seen.retain(|(t, _)| t == newest);
        } else {
            seen.clear();
        }
        match response.get("nextToken") {
            Some(token) => {
                params.insert("nextToken".to_string(), token.clone());
            }
            None => {
                if let Some(newest) = newest {
                    params.insert("startTime".to_string(), newest);
                }
                params.shift_remove("nextToken");
                for _ in 0..50 {
                    if INTERRUPTED.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

fn live_tail_args() -> Vec<ArgDef> {
    let list = serde_json::json!({"type": "array", "items": {"type": "string"}});
    vec![
        ArgDef::new("log-group-identifiers")
            .required()
            .nargs(Nargs::OneOrMore)
            .schema(list.clone()),
        ArgDef::new("log-stream-names")
            .nargs(Nargs::OneOrMore)
            .schema(list.clone()),
        ArgDef::new("log-stream-name-prefixes")
            .nargs(Nargs::OneOrMore)
            .schema(list),
        ArgDef::new("log-event-filter-pattern").type_name("string"),
        ArgDef::new("mode")
            .default_str("print-only")
            .choices(&["interactive", "print-only"]),
    ]
}

/// `StartLiveTailCommand._run_main`: the request and the checks before the
/// session. Reading the event stream of the response is not supported.
pub(super) fn start_live_tail(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let Some(a) = basic::parse(ctx, &["logs", "start-live-tail"], &live_tail_args(), args)? else {
        return Ok(ExitCode::Success);
    };
    let client = Client::new(ctx, "logs")?;
    let mut kwargs = Map::new();
    kwargs.insert(
        "logGroupIdentifiers".to_string(),
        a.get("log-group-identifiers")
            .cloned()
            .unwrap_or(Value::Null),
    );
    for (arg, key) in [
        ("log-stream-names", "logStreamNames"),
        ("log-stream-name-prefixes", "logStreamNamePrefixes"),
        ("log-event-filter-pattern", "logEventFilterPattern"),
    ] {
        if let Some(v) = a.get(arg) {
            kwargs.insert(key.to_string(), v.clone());
        }
    }
    client.call("StartLiveTail", &Value::Map(kwargs))?;
    if a.str("mode").as_deref() == Some("interactive") {
        let allowed = match ctx.color() {
            Color::On => true,
            Color::Off => false,
            Color::Auto => ctx.terminal.stdout_is_tty,
        };
        if !allowed {
            return Err(basic::param_validation(
                ctx,
                "Color is not allowed by your output. Use print-only mode or enable color.",
            ));
        }
    }
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(ts: i64, message: &str) -> Value {
        let mut m = Map::new();
        m.insert("logStreamName".into(), Value::str("stream"));
        m.insert("timestamp".into(), Value::int(ts));
        m.insert("message".into(), Value::str(message));
        m.insert("ingestionTime".into(), Value::int(ts));
        m.insert("eventId".into(), Value::str("1"));
        Value::Map(m)
    }

    #[test]
    fn formats() {
        let e = event(1_700_000_000_123, "hello  \n\n");
        assert_eq!(
            format_event(&e, Format::Detailed, false).unwrap(),
            "2023-11-14T22:13:20.123000+00:00 stream hello\n"
        );
        assert_eq!(
            format_event(&e, Format::Short, true).unwrap(),
            "\x1b[32m2023-11-14T22:13:20\x1b[0m hello\n"
        );
        let e = event(1_700_000_000_000, "{\"a\": [1, \"\u{e9}\"]}");
        assert_eq!(
            format_event(&e, Format::Json, false).unwrap(),
            "2023-11-14T22:13:20+00:00 stream \n{\n    \"a\": [\n        1,\n        \"\\u00e9\"\n    ]\n}\n"
        );
    }

    #[test]
    fn absolute_since() {
        assert_eq!(
            to_epoch_millis("2023-11-14T22:13:20.5Z").unwrap(),
            1_700_000_000_500
        );
        assert_eq!(to_epoch_millis("1700000000").unwrap(), 1_700_000_000_000);
    }
}
