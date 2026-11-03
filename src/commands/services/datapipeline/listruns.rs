//! `datapipeline list-runs` (`datapipeline/__init__.py` `ListRunsCommand`,
//! `QueryArgBuilder`, `listrunsformatter.py`).

use crate::commands::services::basic::{self, ArgDef};
use crate::commands::services::exec::Client;
use crate::driver::{Context, display_response};
use crate::error::{CliError, ExitCode};
use crate::output::Response;
use crate::value::{Map, Value};

const VALID_STATUS: &[&str] = &[
    "waiting",
    "pending",
    "cancelled",
    "running",
    "finished",
    "failed",
    "waiting_for_runner",
    "waiting_on_dependencies",
    "shutting_down",
];

const MAX_ITEMS_PER_DESCRIBE: usize = 100;

fn selector(field: &str, op: &str, values: Vec<Value>) -> Value {
    Value::from_json(&serde_json::json!({
        "fieldName": field,
        "operator": {"type": op, "values": []},
    }))
    .with_values(values)
}

trait WithValues {
    fn with_values(self, values: Vec<Value>) -> Value;
}

impl WithValues for Value {
    fn with_values(mut self, values: Vec<Value>) -> Value {
        if let Some(op) = self
            .as_map_mut()
            .and_then(|m| m.get_mut("operator"))
            .and_then(Value::as_map_mut)
        {
            op.insert("values".to_string(), Value::List(values));
        }
        self
    }
}

fn split_strip(s: &str) -> Vec<String> {
    s.split(',').map(|a| a.trim().to_string()).collect()
}

pub(super) fn list_runs(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("pipeline-id").required().type_name("string"),
        ArgDef::new("status"),
        ArgDef::new("start-interval").type_name("string"),
        ArgDef::new("schedule-interval").type_name("string"),
    ];
    let Some(a) = basic::parse(ctx, &["datapipeline", "list-runs"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let pipeline_id = a.str("pipeline-id").unwrap_or_default();
    let client = Client::new(ctx, "datapipeline")?;
    let start = a.str("start-interval").map(|s| split_strip(&s));
    let schedule = a.str("schedule-interval").map(|s| split_strip(&s));
    let status = a.str("status").map(|s| split_strip(&s));
    if let Some(statuses) = &status {
        for s in statuses {
            if !VALID_STATUS.contains(&s.as_str()) {
                return Err(basic::param_validation(
                    ctx,
                    &format!(
                        "Invalid status: {s}, must be one of: {}",
                        VALID_STATUS.join(", ")
                    ),
                ));
            }
        }
    }
    let index = |l: &Vec<String>, i: usize| -> Result<Value, CliError> {
        l.get(i)
            .map(|s| Value::str(s))
            .ok_or_else(|| ctx.general_error(&anyhow::anyhow!("list index out of range")))
    };
    let mut selectors = Vec::new();
    if start.is_none() && schedule.is_none() {
        let end = chrono::Utc::now();
        let begin = end - chrono::Duration::days(4);
        let fmt = "%Y-%m-%dT%H:%M:%S";
        selectors.push(selector(
            "@actualStartTime",
            "BETWEEN",
            vec![
                Value::str(begin.format(fmt).to_string()),
                Value::str(end.format(fmt).to_string()),
            ],
        ));
    } else {
        if let Some(s) = &start {
            selectors.push(selector(
                "@actualStartTime",
                "BETWEEN",
                vec![index(s, 0)?, index(s, 1)?],
            ));
        }
        if let Some(s) = &schedule {
            selectors.push(selector(
                "@scheduledStartTime",
                "BETWEEN",
                vec![index(s, 0)?, index(s, 1)?],
            ));
        }
    }
    if let Some(statuses) = &status {
        selectors.push(selector(
            "@status",
            "EQ",
            statuses
                .iter()
                .map(|s| Value::str(s.to_uppercase()))
                .collect(),
        ));
    }
    let mut query = Map::new();
    query.insert("selectors".to_string(), Value::List(selectors));
    let mut params = Map::new();
    params.insert("pipelineId".to_string(), Value::str(&pipeline_id));
    params.insert("sphere".to_string(), Value::str("INSTANCE"));
    params.insert("query".to_string(), Value::Map(query));
    let pages = client.paginate("QueryObjects", &Value::Map(params))?;
    let ids: Vec<Value> = pages
        .iter()
        .filter_map(|p| p.get("ids").and_then(Value::as_list))
        .flatten()
        .cloned()
        .collect();
    let mut objects = Vec::new();
    for chunk in ids.chunks(MAX_ITEMS_PER_DESCRIBE) {
        let mut p = Map::new();
        p.insert("pipelineId".to_string(), Value::str(&pipeline_id));
        p.insert("objectIds".to_string(), Value::List(chunk.to_vec()));
        let result = client.call("DescribeObjects", &Value::Map(p))?;
        if let Some(l) = result.get("pipelineObjects").and_then(Value::as_list) {
            objects.extend(l.iter().cloned());
        }
    }
    let mut converted = convert_described_objects(&objects);
    converted.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v.get("@scheduledStartTime").map(crate::pyfmt::py_str),
                v.get("name").map(crate::pyfmt::py_str),
            )
        };
        key(a).cmp(&key(b))
    });
    let list = Value::List(converted);
    if ctx.globals.output.is_some() {
        display_response(ctx, "list-runs", Response::Single(list))?;
        return Ok(ExitCode::Success);
    }
    let data = match &ctx.globals.query {
        Some(q) => crate::jmes::search(q, &list).map_err(|e| ctx.general_error(&e))?,
        None => list,
    };
    let text = format_runs(&data).map_err(|e| ctx.general_error(&anyhow::anyhow!(e)))?;
    basic::print(&text);
    Ok(ExitCode::Success)
}

/// `convert_described_objects`.
fn convert_described_objects(objects: &[Value]) -> Vec<Value> {
    objects
        .iter()
        .map(|obj| {
            let mut m = Map::new();
            m.insert(
                "@id".to_string(),
                obj.get("id").cloned().unwrap_or(Value::Null),
            );
            m.insert(
                "name".to_string(),
                obj.get("name").cloned().unwrap_or(Value::Null),
            );
            for field in obj
                .get("fields")
                .and_then(Value::as_list)
                .into_iter()
                .flatten()
            {
                let key = field
                    .get("key")
                    .map(crate::pyfmt::py_str)
                    .unwrap_or_default();
                let value = field
                    .get("stringValue")
                    .or_else(|| field.get("refValue"))
                    .cloned()
                    .unwrap_or(Value::Null);
                m.insert(key, value);
            }
            Value::Map(m)
        })
        .collect()
}

/// Python `'%-W.Ps' % value`.
fn col(value: &str, width: usize) -> String {
    let truncated: String = value.chars().take(width).collect();
    format!("{truncated:<width$}")
}

/// `ListRunsFormatter._format_response`.
fn format_runs(data: &Value) -> Result<String, String> {
    let mut out = String::new();
    out.push_str(&format!(
        "       {}  {}  {}\n",
        col("Name", 50),
        col("Scheduled Start", 19),
        col("Status", 23)
    ));
    let second = format!(
        "       {}  {}  {}",
        col("ID", 50),
        col("Started", 19),
        col("Ended", 19)
    );
    out.push_str(&second);
    out.push('\n');
    out.push_str(&"-".repeat(second.chars().count()));
    out.push('\n');
    let items = match data {
        Value::List(l) => l.clone(),
        _ => Vec::new(),
    };
    for (i, obj) in items.iter().enumerate() {
        let get = |k: &str| obj.get(k).map(crate::pyfmt::py_str).unwrap_or_default();
        let parent = obj
            .get("@componentParent")
            .map(crate::pyfmt::py_str)
            .ok_or_else(|| "'@componentParent'".to_string())?;
        let id = obj
            .get("@id")
            .map(crate::pyfmt::py_str)
            .ok_or_else(|| "'@id'".to_string())?;
        out.push_str(&format!(
            "{:>4}.  {}  {}  {}\n",
            i + 1,
            col(&parent, 50),
            col(&get("@scheduledStartTime"), 19),
            col(&get("@status"), 23)
        ));
        out.push_str(&format!(
            "       {}  {}  {}\n\n",
            col(&id, 50),
            col(&get("@actualStartTime"), 19),
            col(&get("@actualEndTime"), 19)
        ));
    }
    Ok(out)
}
