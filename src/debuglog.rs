//! `--debug` logging (`CLIDriver._set_logging`, `set_stream_logger` with
//! `LOG_FORMAT`): records of the main events written to stderr in the
//! format of Python's `logging` handlers.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// One record: `%(asctime)s - %(threadName)s - %(name)s - %(levelname)s -
/// %(message)s`.
pub fn log(name: &str, level: &str, message: &str) {
    if !enabled() {
        return;
    }
    let now = chrono::Local::now();
    let line = format!(
        "{},{:03} - MainThread - {name} - {level} - {message}\n",
        now.format("%Y-%m-%d %H:%M:%S"),
        now.timestamp_subsec_millis()
    );
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(line.as_bytes());
    let _ = err.flush();
}

pub fn debug(name: &str, message: &str) {
    log(name, "DEBUG", message);
}

/// `repr()` of a list of strings.
pub fn list_repr(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| crate::pyfmt::str_repr(s)).collect();
    format!("[{}]", parts.join(", "))
}

/// `repr()` of a dict of strings (response headers).
pub fn headers_repr(headers: &[(String, String)]) -> String {
    let parts: Vec<String> = headers
        .iter()
        .map(|(k, v)| {
            format!(
                "{}: {}",
                crate::pyfmt::str_repr(k),
                crate::pyfmt::str_repr(v)
            )
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}
