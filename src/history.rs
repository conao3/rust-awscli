//! CLI history (`botocore/history.py` `HistoryRecorder`,
//! `awscli/customizations/history` `attach_history_handler`, `db.py`
//! `DatabaseConnection`, `DatabaseRecordWriter`, `RecordBuilder`,
//! `DatabaseHistoryHandler`): with `cli_history = enabled` each command
//! writes its events to an SQLite database that `aws history` reads.

pub(crate) mod minidom;
pub(crate) mod redact;
pub(crate) mod show;

use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Config;
use crate::value::{Map, Value};

/// `HISTORY_FILENAME_ENV_VAR`.
const HISTORY_FILENAME_ENV_VAR: &str = "AWS_CLI_HISTORY_FILE";

/// `RecordBuilder._REQUEST_LIFECYCLE_EVENTS`.
const REQUEST_LIFECYCLE_EVENTS: &[&str] = &[
    "API_CALL",
    "HTTP_REQUEST",
    "HTTP_RESPONSE",
    "PARSED_RESPONSE",
];

static ENABLED: AtomicBool = AtomicBool::new(false);
static RECORDER: Mutex<Option<Recorder>> = Mutex::new(None);

thread_local! {
    /// `RecordBuilder._locals.request_id`.
    static REQUEST_ID: RefCell<Option<String>> = const { RefCell::new(None) };
}

struct Recorder {
    connection: rusqlite::Connection,
    command_id: Option<String>,
}

/// `DEFAULT_HISTORY_FILENAME`, or `AWS_CLI_HISTORY_FILE`.
pub(crate) fn filename() -> String {
    std::env::var(HISTORY_FILENAME_ENV_VAR).unwrap_or_else(|_| {
        crate::config::ini::expand_path("~/.aws/cli/history/history.db", &|k| std::env::var(k).ok())
    })
}

/// `DatabaseConnection.__init__`: connect, set the permissions of the
/// files that exist, create the `records` table, and try WAL.
pub(crate) fn connect(filename: &str) -> rusqlite::Result<rusqlite::Connection> {
    let connection = rusqlite::Connection::open(filename)?;
    for suffix in ["", "-wal", "-shm"] {
        let path = format!("{filename}{suffix}");
        if std::path::Path::new(&path).exists() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    connection.execute(
        "CREATE TABLE IF NOT EXISTS records (
          id TEXT,
          request_id TEXT,
          source TEXT,
          event_type TEXT,
          timestamp INTEGER,
          payload TEXT
        )",
        [],
    )?;
    let _ = connection.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()));
    Ok(connection)
}

/// `attach_history_handler` on `session-initialized` for the command
/// `command` (`_should_enable_cli_history`).
pub(crate) fn attach(config: &Config, command: Option<&str>) {
    if command == Some("history") {
        return;
    }
    let Ok(scoped) = config.scoped_config() else {
        return;
    };
    if scoped.get("cli_history").and_then(|v| v.as_str()) != Some("enabled") {
        return;
    }
    let filename = filename();
    if let Some(dir) = std::path::Path::new(&filename).parent()
        && !dir.is_dir()
    {
        let _ = std::fs::create_dir_all(dir);
    }
    match connect(&filename) {
        Ok(connection) => {
            *RECORDER.lock().unwrap() = Some(Recorder {
                connection,
                command_id: None,
            });
            ENABLED.store(true, Ordering::Relaxed);
        }
        Err(_) => {
            crate::commands::services::basic::eprint(&format!(
                "Warning: Unable to record CLI history. Check file permissions for {filename}\n"
            ));
        }
    }
}

static STREAMING_FILE: Mutex<Option<String>> = Mutex::new(None);
static API_PARAMS: Mutex<Option<ApiParams>> = Mutex::new(None);

/// The arguments of the client method the command calls, as given before
/// `provide-client-params`.
struct ApiParams {
    operation: String,
    params: Value,
    recorded: bool,
}

/// The parameters the `API_CALL` of the command's calls of `operation`
/// records.
pub(crate) fn set_api_params(operation: &str, params: Option<Value>) {
    *API_PARAMS.lock().unwrap() = params.map(|params| ApiParams {
        operation: operation.to_string(),
        params,
        recorded: false,
    });
}

/// `params` of the `API_CALL` of a call of `operation` with `params`. The
/// first call records the arguments as given. A paginator or waiter calls
/// the method again with `**kwargs` of the same dict: the top-level
/// members are those given (`base64_decode_input_blobs` replaced them in
/// the copy), the nested values are what the handler left in place.
pub(crate) fn call_params(operation: &crate::model::Operation<'_>, params: &Value) -> Value {
    let mut guard = API_PARAMS.lock().unwrap();
    let Some(given) = guard.as_mut().filter(|g| g.operation == operation.name) else {
        return params.clone();
    };
    if !given.recorded {
        given.recorded = true;
        return given.params.clone();
    }
    let mut params = params.clone();
    if let Some(input) = operation.input()
        && let Some(map) = params.as_map_mut()
        && let Some(original) = given.params.as_map()
    {
        for (k, v) in map.iter_mut() {
            if let Some(o) = original.get(k)
                && input
                    .member_shape(k)
                    .is_some_and(|s| s.type_name() == "blob")
            {
                *v = o.clone();
            }
        }
    }
    params
}

/// The file a streaming blob parameter was opened from
/// (`open(path, 'rb')`).
pub(crate) fn set_streaming_file(path: &str) {
    *STREAMING_FILE.lock().unwrap() = Some(path.to_string());
}

/// `params` of `API_CALL` for `operation`: a streaming blob parameter is
/// the file object the CLI opened, which `PayloadSerializer` writes as its
/// `repr()`.
pub(crate) fn api_params(operation: &crate::model::Operation<'_>, params: &Value) -> Value {
    let mut params = params.clone();
    let path = STREAMING_FILE.lock().unwrap().clone();
    if let Some(path) = path
        && let Some(input) = operation.input()
        && let Some(member) = input.shape_get("payload").and_then(|p| p.as_str())
        && input
            .member_shape(member)
            .is_some_and(|s| s.type_name() == "blob")
        && let Some(map) = params.as_map_mut()
        && let Some(v @ Value::Bytes(_)) = map.get_mut(member)
    {
        *v = Value::Str(format!(
            "<_io.BufferedReader name={}>",
            crate::pyfmt::str_repr(&path)
        ));
    }
    params
}

/// Whether events are recorded.
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// `HistoryRecorder.record(event_type, payload, source)`.
pub(crate) fn record(event_type: &str, payload: Value, source: &str) {
    if !enabled() {
        return;
    }
    let mut guard = RECORDER.lock().unwrap();
    let Some(recorder) = guard.as_mut() else {
        return;
    };
    let command_id = recorder
        .command_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    if event_type == "API_CALL" {
        REQUEST_ID.with(|r| *r.borrow_mut() = Some(uuid::Uuid::new_v4().to_string()));
    }
    let request_id = if REQUEST_LIFECYCLE_EVENTS.contains(&event_type) {
        REQUEST_ID.with(|r| r.borrow().clone())
    } else {
        None
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    let payload = crate::pyfmt::json_dumps_default(&serializable(redact::redact(payload)));
    let _ = recorder.connection.execute(
        "INSERT INTO records(
            id, request_id, source, event_type, timestamp, payload)
        VALUES (?,?,?,?,?,?) ",
        rusqlite::params![
            command_id, request_id, source, event_type, timestamp, payload
        ],
    );
}

/// `PayloadSerializer`: bytes become their UTF-8 text, or
/// `<Byte sequence>` when they are not UTF-8.
fn serializable(value: Value) -> Value {
    match value {
        Value::Bytes(b) => match String::from_utf8(b) {
            Ok(s) => Value::Str(s),
            Err(_) => Value::Str("<Byte sequence>".to_string()),
        },
        Value::List(l) => Value::List(l.into_iter().map(serializable).collect()),
        Value::Map(m) => Value::Map(m.into_iter().map(|(k, v)| (k, serializable(v))).collect()),
        v => v,
    }
}

/// A headers dict: the values of repeated names joined with `, `
/// (`HTTPHeaderDict`), in the order the names first appear.
pub(crate) fn headers(headers: &[(String, String)]) -> Value {
    let mut out = Map::new();
    for (k, v) in headers {
        let existing = out.keys().find(|e| e.eq_ignore_ascii_case(k)).cloned();
        match existing {
            Some(e) => {
                if let Some(Value::Str(s)) = out.get_mut(&e) {
                    s.push_str(", ");
                    s.push_str(v);
                }
            }
            None => {
                out.insert(k.clone(), Value::Str(v.clone()));
            }
        }
    }
    Value::Map(out)
}
