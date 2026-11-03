//! Rendering errors for stderr (`awscli/errorhandler.py`,
//! `awscli/errorformat.py`, `botocore/exceptions.py`).

use crate::value::Value;

/// A service error to report (`botocore.exceptions.ClientError`).
#[derive(Debug, Clone)]
pub struct ClientErrorInfo {
    pub operation_name: String,
    pub code: String,
    pub message: String,
    /// `(reached max retries: N)` is appended when the request was retried
    /// until the attempt limit; this is `N`.
    pub max_retries_reached: Option<u32>,
    /// The full parsed error response (for the `json` / `yaml` / `text` /
    /// `table` / `enhanced` error formats).
    pub parsed: Value,
}

/// Render a client error in `error_format` (`legacy`, `json`, `yaml`,
/// `text`, `table`, `enhanced`), exactly as the AWS CLI writes it to
/// stderr.
pub fn render_client_error(info: &ClientErrorInfo, error_format: &str) -> String {
    let _ = (info, error_format);
    todo!("errors::render_client_error")
}

/// `errorformat.write_error`: `"\naws: [ERROR]: <message>\n"` (with the
/// exact surrounding newlines of the AWS CLI).
pub fn render_general_error(message: &str) -> String {
    let _ = message;
    todo!("errors::render_general_error")
}
