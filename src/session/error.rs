//! Errors raised while configuring a session, resolving credentials,
//! signing, and sending (`botocore/exceptions.py`), with the exit code and
//! structured error info the AWS CLI's handlers give them
//! (`awscli/errorhandler.py`).

use std::fmt;

use crate::error::{CliError, ExitCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// `ProfileNotFound`.
    ProfileNotFound,
    /// `ConfigParseError`.
    ConfigParse,
    /// `InvalidConfigError`, `InvalidRetryModeError`,
    /// `InvalidMaxRetryAttemptsError`, a failed `int()` conversion, and
    /// other configuration value errors handled by the general handler.
    InvalidConfig,
    /// `InvalidRegionError` (a `ValueError`; general handler).
    InvalidRegion,
    /// `NoCredentialsError` (`NoCredentialsErrorHandler`, exit 253).
    NoCredentials,
    /// `NoRegionError` (`NoRegionErrorHandler`, exit 253).
    NoRegion,
    /// `PartialCredentialsError`.
    PartialCredentials,
    /// `NoAuthTokenError`.
    NoAuthToken,
    /// Other credential provider failures (assume role, SSO, process).
    CredentialProvider,
    /// `UnknownSignatureVersionError` / `UnsupportedSignatureVersionError`.
    Signature,
    /// `EndpointConnectionError` and the other subclasses of
    /// `ConnectionError` (all retryable).
    Connection,
    /// `ConnectTimeoutError` (a `ConnectionError`).
    ConnectTimeout,
    /// `ReadTimeoutError` (an `HTTPClientError`).
    ReadTimeout,
    /// `ConnectionClosedError` (an `HTTPClientError`).
    ConnectionClosed,
    /// `SSLError` (a `ConnectionError`).
    Ssl,
    /// `ProxyConnectionError` (a `ConnectionError`).
    Proxy,
    /// `HTTPClientError`.
    HttpClient,
    /// `ValueError("Invalid endpoint: ...")` and similar.
    General,
    /// A `ClientError` raised while loading credentials (an STS, SSO, or
    /// Sign-In call failed); `message` is `str(ClientError)`.
    ServiceError,
    /// `awscli.customizations.exceptions.ConfigurationError`
    /// (`ConfigurationErrorHandler`, exit 253, code `Configuration`).
    Configuration,
    /// `ParamValidationError` from botocore for an internal call
    /// (`ParamValidationErrorsHandler`, exit 252).
    ParamValidation,
}

/// An error with the exact text of `str(exception)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError {
    pub kind: ErrorKind,
    pub message: String,
    /// Raised when deferred credentials were first used (an STS, SSO, or
    /// Sign-In call, or the MFA prompt) rather than while the provider
    /// chain was loaded.
    pub deferred: bool,
}

impl SessionError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> SessionError {
        SessionError {
            kind,
            message: message.into(),
            deferred: false,
        }
    }

    /// Mark the error as raised by deferred credentials.
    pub fn into_deferred(mut self) -> SessionError {
        self.deferred = true;
        self
    }

    pub fn profile_not_found(profile: &str) -> SessionError {
        SessionError::new(
            ErrorKind::ProfileNotFound,
            format!("The config profile ({profile}) could not be found"),
        )
    }

    pub fn config_parse(path: &str) -> SessionError {
        SessionError::new(
            ErrorKind::ConfigParse,
            format!("Unable to parse config file: {path}"),
        )
    }

    pub fn no_credentials() -> SessionError {
        SessionError::new(ErrorKind::NoCredentials, "Unable to locate credentials")
    }

    /// Whether botocore's standard retry mode retries this exception
    /// (`TransientRetryableChecker._TRANSIENT_EXCEPTION_CLS`:
    /// `ConnectionError` and `HTTPClientError` subclasses).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Connection
                | ErrorKind::ConnectTimeout
                | ErrorKind::ReadTimeout
                | ErrorKind::ConnectionClosed
                | ErrorKind::Ssl
                | ErrorKind::Proxy
                | ErrorKind::HttpClient
        )
    }

    /// `RetryQuotaChecker._TIMEOUT_EXCEPTIONS`.
    pub fn is_timeout(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::ConnectTimeout | ErrorKind::ReadTimeout
        )
    }

    /// The exit code the AWS CLI's exception handler chain returns.
    pub fn exit_code(&self) -> ExitCode {
        match self.kind {
            ErrorKind::NoCredentials | ErrorKind::NoRegion | ErrorKind::Configuration => {
                ExitCode::Config
            }
            ErrorKind::ServiceError => ExitCode::Service,
            ErrorKind::ParamValidation => ExitCode::Usage,
            _ => ExitCode::General,
        }
    }

    /// `_extract_error_info` of the handler that catches this error:
    /// `(Code, Message)` for the structured error formats, or `None` for
    /// errors that go through `GeneralExceptionHandler`.
    pub fn error_info(&self) -> Option<(String, String)> {
        match self.kind {
            ErrorKind::NoCredentials => Some((
                "NoCredentials".to_string(),
                format!(
                    "{}. You can configure credentials by running \"aws login\".",
                    self.message
                ),
            )),
            ErrorKind::NoRegion => Some((
                "NoRegion".to_string(),
                format!(
                    "{} You can also configure your region by running \"aws configure\".",
                    self.message
                ),
            )),
            ErrorKind::Configuration => Some(("Configuration".to_string(), self.message.clone())),
            ErrorKind::ParamValidation => {
                Some(("ParamValidation".to_string(), self.message.clone()))
            }
            _ => None,
        }
    }

    /// The stderr text for the `legacy` and `enhanced` error formats
    /// (`FilteredExceptionHandler._do_handle_exception` with
    /// `errorformat.write_error`). Other formats render `error_info()`
    /// with the output formatters.
    pub fn render(&self, error_format: &str) -> String {
        let message = match (self.error_info(), error_format) {
            (Some((_, message)), "legacy") => message,
            (Some((code, message)), _) => format!("An error occurred ({code}): {message}"),
            (None, _) => self.message.clone(),
        };
        let message = message.trim();
        if message.is_empty() {
            "\nawsr: [ERROR]:\n".to_string()
        } else {
            format!("\nawsr: [ERROR]: {message}\n")
        }
    }

    /// A `CliError` with the `enhanced` (default) rendering.
    pub fn to_cli_error(&self) -> CliError {
        CliError::new(self.exit_code(), self.render("enhanced"))
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SessionError {}

impl From<SessionError> for CliError {
    fn from(e: SessionError) -> CliError {
        e.to_cli_error()
    }
}

/// Python's `repr()` of a `str` (used in `int()` error messages).
pub fn py_repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `int(value)` for a `str`: surrounding whitespace, a sign, and
/// single underscores between digits are accepted.
pub fn py_int(value: &str) -> Result<i64, SessionError> {
    let err = || {
        SessionError::new(
            ErrorKind::InvalidConfig,
            format!(
                "invalid literal for int() with base 10: {}",
                py_repr_str(value)
            ),
        )
    };
    let t = value.trim();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return Err(err());
    }
    let n: i64 = digits.replace('_', "").parse().map_err(|_| err())?;
    Ok(if neg { -n } else { n })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_conversion() {
        assert_eq!(py_int(" 3 ").unwrap(), 3);
        assert_eq!(py_int("1_0").unwrap(), 10);
        assert_eq!(
            py_int("x").unwrap_err().message,
            "invalid literal for int() with base 10: 'x'"
        );
        assert_eq!(
            py_int("it's").unwrap_err().message,
            "invalid literal for int() with base 10: \"it's\""
        );
    }

    #[test]
    fn rendering() {
        assert_eq!(
            SessionError::no_credentials().render("enhanced"),
            "\nawsr: [ERROR]: An error occurred (NoCredentials): Unable to locate credentials. \
             You can configure credentials by running \"aws login\".\n"
        );
        assert_eq!(
            SessionError::profile_not_found("x").to_cli_error().message,
            "\nawsr: [ERROR]: The config profile (x) could not be found\n"
        );
    }
}
