//! Rendering parse failures for stderr (`awscli/errorhandler.py`
//! `ParamValidationErrorsHandler`, `UnknownArgumentErrorHandler`,
//! `GeneralExceptionHandler`; `awscli/errorformat.py`).

use std::io::IsTerminal;

use super::{Fail, GlobalArgs};
use crate::config::ConfigLookup;
use crate::error::{CliError, ExitCode};
use crate::output::errors::render_general_error;
use crate::output::{table, text, yaml};
use crate::pyfmt;
use crate::value::{Map, Value};

/// `awscli/argparser.py` `USAGE`.
pub(crate) const USAGE: &str = "awsr [options] <command> <subcommand> [<subcommand> ...] [parameters]\nTo see help text, you can run:\n\n  awsr help\n  awsr <command> help\n  awsr <command> <subcommand> help\n";

/// `CLIArgParser.format_usage()`.
pub(crate) fn format_usage() -> String {
    format!("usage: {USAGE}")
}

/// `CLIArgParser.error(message)`: the `ArgParseException` text.
pub(crate) fn argparse_error(message: &str) -> Fail {
    Fail::ParamValidation(format!("{message}\n\n{}", format_usage()))
}

/// `FilteredExceptionHandler._resolve_error_format`.
fn resolve_error_format(globals: Option<&GlobalArgs>, config: Option<&dyn ConfigLookup>) -> String {
    if let Some(f) = globals.and_then(|g| g.cli_error_format.as_deref())
        && !f.is_empty()
    {
        return f.to_lowercase();
    }
    if let Some(f) = config.and_then(|c| c.config_variable("cli_error_format"))
        && !f.is_empty()
    {
        return f.to_lowercase();
    }
    "enhanced".to_string()
}

pub(crate) fn render(
    fail: Fail,
    globals: Option<&GlobalArgs>,
    config: Option<&dyn ConfigLookup>,
) -> CliError {
    match fail {
        Fail::ParamValidation(message) => {
            let format = resolve_error_format(globals, config);
            let color = globals.and_then(|g| g.color.as_deref()).unwrap_or("auto");
            CliError::new(ExitCode::Usage, param_validation(&message, &format, color))
        }
        Fail::Unknown(message) => CliError::new(
            ExitCode::Usage,
            format!("\n{}\n{}", format_usage(), render_general_error(&message)),
        ),
        Fail::Python(message) | Fail::General(message) => {
            CliError::new(ExitCode::General, render_general_error(&message))
        }
    }
}

/// `ParamValidationErrorsHandler._do_handle_exception` for the error
/// formats.
fn param_validation(message: &str, format: &str, color: &str) -> String {
    let formatted = format!("An error occurred (ParamValidation): {message}");
    let mut format = format.to_string();
    if format == "legacy" {
        return render_general_error(message);
    }
    if !matches!(
        format.as_str(),
        "json" | "yaml" | "text" | "table" | "enhanced"
    ) {
        format = "enhanced".to_string();
    }
    if format == "enhanced" {
        return render_general_error(&formatted);
    }
    let mut info = Map::new();
    info.insert("Code".to_string(), Value::str("ParamValidation"));
    info.insert("Message".to_string(), Value::str(message));
    let data = Value::Map(info);
    let mut out = String::new();
    match format.as_str() {
        "json" => {
            out.push_str(&pyfmt::json_dumps_pretty(&data));
            out.push('\n');
        }
        "yaml" => yaml::dump(&data, &mut out),
        "text" => {
            if text::format_text(&data, &mut out).is_err() {
                out.push_str(&render_general_error(&formatted));
            }
        }
        _ => {
            let colorized = match color {
                "on" => true,
                "off" => false,
                _ => std::io::stdout().is_terminal(),
            };
            let styler = if colorized {
                crate::output::colorama::init();
                table::Styler::Colorized
            } else {
                table::Styler::Plain
            };
            if crate::output::render_table("error", &data, styler, false, 80, &mut out).is_err() {
                return render_general_error(message);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(
            param_validation("bad\n", "enhanced", "auto"),
            "\nawsr: [ERROR]: An error occurred (ParamValidation): bad\n"
        );
        assert_eq!(
            param_validation("bad", "legacy", "auto"),
            "\nawsr: [ERROR]: bad\n"
        );
        assert_eq!(
            param_validation("bad", "json", "auto"),
            "{\n    \"Code\": \"ParamValidation\",\n    \"Message\": \"bad\"\n}\n"
        );
    }
}
