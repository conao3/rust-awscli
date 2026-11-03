//! Output formatting (`awscli/formatter.py`, `awscli/table.py`,
//! `awscli/text.py`).

use std::io::Write;

use anyhow::Result;

use crate::paginate::PaginatedResponse;
use crate::value::Value;

pub mod errors;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Text,
    Table,
    Yaml,
    YamlStream,
    Off,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        Some(match s {
            "json" => Format::Json,
            "text" => Format::Text,
            "table" => Format::Table,
            "yaml" => Format::Yaml,
            "yaml-stream" => Format::YamlStream,
            "off" => Format::Off,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Auto,
    On,
    Off,
}

pub struct OutputOptions {
    pub format: Format,
    /// `--query`.
    pub query: Option<String>,
    pub color: Color,
    /// Width of the terminal attached to stdout; `None` when stdout is not
    /// a terminal (the table formatter then uses 80).
    pub terminal_width: Option<usize>,
    /// Whether stdout is a tty (`--color auto`).
    pub stdout_is_tty: bool,
}

pub enum Response {
    Single(Value),
    Paginated(PaginatedResponse),
}

/// Format `response` like the AWS CLI formatter for
/// `command_name` (the operation name, e.g. `DescribeInstances`, which the
/// table formatter uses as the title).
pub fn write_response(
    command_name: &str,
    response: Response,
    options: &OutputOptions,
    out: &mut dyn Write,
) -> Result<()> {
    let _ = (command_name, response, options, out);
    todo!("output::write_response")
}
