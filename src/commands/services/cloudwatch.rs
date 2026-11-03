//! `cloudwatch put-metric-data` (`awscli/customizations/putmetricdata.py`)
//! and the helpers the argument customizations of this group share
//! (`awscli/customizations/utils.py` `validate_mutually_exclusive`,
//! `awscli/utils.py` `split_on_commas`).

use indexmap::IndexMap;

use super::exec;
use crate::cli::argparse::NsVal;
use crate::cli::shorthand::float_of;
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "put-metric-data" => exec::run_operation(ctx, "cloudwatch", name, args, &mut PutMetricData),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `validate_mutually_exclusive(parsed_args, *groups)`.
pub(crate) fn validate_mutually_exclusive(
    ns: &IndexMap<String, NsVal>,
    groups: &[&[&str]],
) -> Result<(), Fail> {
    let group_of = |key: &str| groups.iter().find(|g| g.contains(&key)).copied();
    let set: Vec<&String> = ns
        .iter()
        .filter(|(_, v)| !v.is_none())
        .map(|(k, _)| k)
        .collect();
    if !set.iter().any(|k| group_of(k).is_some()) {
        return Ok(());
    }
    let mut current: Option<&[&str]> = None;
    for key in set {
        let Some(group) = group_of(key) else { continue };
        match current {
            None => current = Some(group),
            Some(c) if c != group => {
                return Err(Fail::ParamValidation(format!(
                    "The key \"{key}\" cannot be specified when one of the following keys are also specified: {}",
                    c.join(", ")
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// `cloudfront.arguments.unique_string(prefix)`.
pub(crate) fn unique_string(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format!("{prefix}-{now}-{}", rand::random_range(1..=1_000_000u32))
}

/// `awscli.utils.split_on_commas` (the quote-free forms).
pub(crate) fn split_on_commas(value: &str) -> Vec<String> {
    if !value.contains(['"', '\\', '\'', ']', '[']) {
        return value.split(',').map(str::to_string).collect();
    }
    let mut parts = vec![String::new()];
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    parts.last_mut().expect("part").push(n);
                }
            }
            ',' => parts.push(String::new()),
            c => parts.last_mut().expect("part").push(c),
        }
    }
    parts
}

/// `key, value = pair.split('=')`.
pub(crate) fn split_pair(pair: &str) -> Result<(String, String), Fail> {
    let parts: Vec<&str> = pair.split('=').collect();
    match parts.as_slice() {
        [k, v] => Ok((k.to_string(), v.to_string())),
        _ => Err(Fail::Python(format!(
            "{} values to unpack (expected 2, got {})",
            if parts.len() > 2 {
                "too many"
            } else {
                "not enough"
            },
            parts.len()
        ))),
    }
}

/// `decimal.Decimal(value)`.
fn decimal(value: &str) -> Result<Value, Fail> {
    float_of(&Value::str(value))
        .map_err(|_| Fail::General("[<class 'decimal.ConversionSyntax'>]".to_string()))
}

pub(crate) fn value_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => crate::pyfmt::py_str(other),
    }
}

struct PutMetricData;

const PROMOTED: &[&str] = &[
    "metric-name",
    "timestamp",
    "unit",
    "value",
    "dimensions",
    "statistic-values",
    "storage-resolution",
];

impl OperationHooks for PutMetricData {
    /// `_promote_args`.
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        if let Some(a) = table.get_mut("metric-data") {
            a.required = false;
        }
        for name in PROMOTED {
            table.insert(
                name.to_string(),
                Arg::custom_argument(name, CustomSpec::default(), false),
            );
        }
    }

    fn args_parsed(&mut self, ns: &IndexMap<String, NsVal>, _: &mut bool) -> Result<(), Fail> {
        validate_mutually_exclusive(
            ns,
            &[
                &["metric_data"],
                &[
                    "metric_name",
                    "timestamp",
                    "unit",
                    "value",
                    "dimensions",
                    "statistic_values",
                ],
            ],
        )
    }

    /// `PutMetricArgument.add_to_params` (`insert_first_element('MetricData')`).
    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        let entry = params
            .entry("MetricData".to_string())
            .or_insert_with(|| Value::List(vec![Value::map()]));
        let first = match entry {
            Value::List(l) => match l.first_mut() {
                Some(Value::Map(m)) => m,
                _ => return Err(Fail::Python("list index out of range".to_string())),
            },
            _ => return Err(Fail::Python("list index out of range".to_string())),
        };
        let text = value_str(&value);
        match name {
            "metric-name" => {
                first.insert("MetricName".to_string(), value);
            }
            "unit" => {
                first.insert("Unit".to_string(), value);
            }
            "timestamp" => {
                first.insert("Timestamp".to_string(), value);
            }
            "value" => {
                first.insert("Value".to_string(), decimal(&text)?);
            }
            "dimensions" => {
                let mut dims = Vec::new();
                for pair in split_on_commas(&text) {
                    let (k, v) = split_pair(&pair)?;
                    let mut m = Map::new();
                    m.insert("Name".to_string(), Value::Str(k));
                    m.insert("Value".to_string(), Value::Str(v));
                    dims.push(Value::Map(m));
                }
                first.insert("Dimensions".to_string(), Value::List(dims));
            }
            "statistic-values" => {
                let mut stats = Map::new();
                for pair in split_on_commas(&text) {
                    let (k, v) = split_pair(&pair)?;
                    stats.insert(k, decimal(&v)?);
                }
                first.insert("StatisticValues".to_string(), Value::Map(stats));
            }
            "storage-resolution" => {
                let v = crate::cli::shorthand::int_of(&value).map_err(Fail::Python)?;
                first.insert("StorageResolution".to_string(), v);
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commas() {
        assert_eq!(split_on_commas("a=1,b=2"), vec!["a=1", "b=2"]);
        assert_eq!(split_on_commas(r"a=1\,2,b"), vec!["a=1,2", "b"]);
        assert!(split_pair("a").is_err());
    }
}
