//! `aws history` (`awscli/customizations/history`: `HistoryCommand`,
//! `commands.py` `HistorySubcommand`, `list.py` `ListCommand`, `show.py`
//! `ShowCommand`, `db.py` `DatabaseRecordReader`).

use crate::cli::argparse::Nargs;
use crate::commands::services::basic::{self, ArgDef};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::history::show::{DetailedFormatter, Record};
use crate::output::Color;
use crate::value::Value;

/// `HistoryCommand.SUBCOMMANDS`.
const SUBCOMMANDS: &[&str] = &["show", "list"];

/// `HistoryCommand.__call__`.
pub(crate) fn run(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    match crate::commands::services::codecommit::parse_subcommand(ctx, args, SUBCOMMANDS)? {
        Some((sub, rest)) if sub == "list" => list(ctx, &rest),
        Some((_, rest)) => show(ctx, &rest),
        None => {
            if basic::parse(ctx, &["history"], &[], args)?.is_none() {
                return Ok(ExitCode::Success);
            }
            Err(basic::usage_error(ctx, &["history"]))
        }
    }
}

fn runtime_error(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

/// `HistorySubcommand._connect_to_history_db`.
fn connect(ctx: &Context) -> Result<rusqlite::Connection, CliError> {
    let filename = crate::history::filename();
    if !std::path::Path::new(&filename).exists() {
        return Err(runtime_error(
            ctx,
            "Could not locate history. Make sure cli_history is set to enabled in the ~/.aws/config file",
        ));
    }
    crate::history::connect(&filename).map_err(|e| runtime_error(ctx, &e.to_string()))
}

/// `ListCommand._run_main` with `TextFormatter`.
fn list(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    if basic::parse(ctx, &["history", "list"], &[], args)?.is_none() {
        return Ok(ExitCode::Success);
    }
    let connection = connect(ctx)?;
    let rows = query(
        ctx,
        &connection,
        "SELECT a.id AS id_a, b.id AS id_b, a.timestamp as timestamp, a.payload AS args, b.payload AS rc FROM records a, records b where a.event_type == 'CLI_ARGUMENTS' AND b.event_type = 'CLI_RC' AND id_a == id_b ORDER BY timestamp DESC",
        &[],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            ))
        },
    )?;
    if rows.is_empty() {
        return Err(runtime_error(
            ctx,
            "No commands were found in your history. Make sure you have enabled history mode by adding \"cli_history = enabled\" to the config file.",
        ));
    }
    let mut out = String::new();
    for (id, timestamp, args, rc) in rows {
        out.push_str(&pad(&id, 38));
        out.push_str(&pad(&list_time(timestamp), 24));
        out.push_str(&pad(&list_args(&args), 50));
        out.push_str(&rc);
        out.push('\n');
    }
    crate::driver::write_paged(ctx, out.as_bytes(), "SR")?;
    Ok(ExitCode::Success)
}

/// `{:<width}` of `str.format`.
fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    format!("{text}{}", " ".repeat(width.saturating_sub(len)))
}

/// `TextFormatter._format_time`.
fn list_time(ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d %I:%M:%S %p").to_string(),
        None => ms.to_string(),
    }
}

/// `TextFormatter._format_args`.
fn list_args(args: &str) -> String {
    let words: Vec<String> = match crate::cli::pyjson::loads(args) {
        Ok(Value::List(l)) => l.iter().take(2).map(crate::pyfmt::py_str).collect(),
        _ => Vec::new(),
    };
    let formatted = words.join(" ");
    if formatted.chars().count() >= 50 {
        let head: String = formatted.chars().take(46).collect();
        return format!("{head}...");
    }
    formatted
}

/// `ShowCommand.ARG_TABLE`.
fn show_args() -> Vec<ArgDef> {
    vec![
        ArgDef::new("command_id")
            .positional()
            .nargs(Nargs::Optional)
            .default_str("latest"),
        ArgDef::new("include").nargs(Nargs::OneOrMore),
        ArgDef::new("exclude").nargs(Nargs::OneOrMore),
        ArgDef::new("format")
            .choices(&["detailed"])
            .default_str("detailed"),
    ]
}

/// `ShowCommand._run_main`.
fn show(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let Some(parsed) = basic::parse(ctx, &["history", "show"], &show_args(), args)? else {
        return Ok(ExitCode::Success);
    };
    let connection = connect(ctx)?;
    let include = parsed.str_list("include");
    let exclude = parsed.str_list("exclude");
    if include.as_ref().is_some_and(|i| !i.is_empty())
        && exclude.as_ref().is_some_and(|e| !e.is_empty())
    {
        return Err(basic::param_validation(
            ctx,
            "Either --exclude or --include can be provided but not both",
        ));
    }
    let colorize = match ctx.color() {
        Color::On => true,
        Color::Off => false,
        Color::Auto => ctx.terminal.stdout_is_tty,
    };
    if colorize {
        crate::output::colorama::init();
    }
    let command_id = parsed
        .str("command_id")
        .unwrap_or_else(|| "latest".to_string());
    let records = if command_id == "latest" {
        query(
            ctx,
            &connection,
            "SELECT * FROM records WHERE id = (SELECT id FROM records WHERE timestamp = (SELECT max(timestamp) FROM records)) ORDER BY timestamp;",
            &[],
            record,
        )?
    } else {
        query(
            ctx,
            &connection,
            "SELECT * from records where id = ? ORDER BY timestamp",
            &[&command_id],
            record,
        )?
    };
    let mut formatter = DetailedFormatter::new(include, exclude, colorize);
    let mut out = String::new();
    for r in &records {
        formatter.display(r, &mut out);
    }
    crate::driver::write_paged(ctx, out.as_bytes(), "FRX")?;
    Ok(ExitCode::Success)
}

/// `DatabaseRecordReader._row_factory` for the columns of `records`.
fn record(row: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    let payload: Option<String> = row.get(5)?;
    Ok(Record {
        request_id: row.get(1)?,
        event_type: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        timestamp: row.get::<_, Option<i64>>(4)?.unwrap_or_default(),
        payload: payload
            .as_deref()
            .and_then(|p| crate::cli::pyjson::loads(p).ok())
            .unwrap_or_default(),
    })
}

fn query<T>(
    ctx: &Context,
    connection: &rusqlite::Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, CliError> {
    let fail = |e: rusqlite::Error| runtime_error(ctx, &e.to_string());
    let mut statement = connection.prepare(sql).map_err(fail)?;
    let rows = statement.query_map(params, map).map_err(fail)?;
    rows.collect::<rusqlite::Result<Vec<T>>>().map_err(fail)
}
