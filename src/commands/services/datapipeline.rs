//! `datapipeline` customizations (`awscli/customizations/datapipeline/`):
//! the pipeline definition arguments of `put-pipeline-definition` and
//! `activate-pipeline` (`translator.py`), and the `list-runs` and
//! `create-default-roles` commands.

mod listruns;

use super::exec;
use crate::cli::argparse::Nargs;
use crate::cli::pyjson;
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "put-pipeline-definition" => exec::run_operation(
            ctx,
            "datapipeline",
            name,
            args,
            &mut Definition { put: true },
        ),
        "activate-pipeline" => exec::run_operation(
            ctx,
            "datapipeline",
            name,
            args,
            &mut Definition { put: false },
        ),
        "list-runs" => listruns::list_runs(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `PipelineDefinitionError(msg, definition)`.
fn definition_error(msg: &str) -> Fail {
    Fail::ParamValidation(format!("Error in pipeline definition: {msg}\n"))
}

/// `json.dumps(element)`.
fn dumps(v: &Value) -> String {
    crate::pyfmt::json_dumps_default(v)
}

/// `_convert_single_field`.
fn convert_single_field(key: &str, value: &Value) -> Value {
    let mut field = Map::new();
    field.insert("key".to_string(), Value::str(key));
    match value {
        Value::Map(m) if m.len() == 1 && m.contains_key("ref") => {
            field.insert("refValue".to_string(), m["ref"].clone());
        }
        other => {
            field.insert("stringValue".to_string(), other.clone());
        }
    }
    Value::Map(field)
}

/// `_parse_each_field`.
fn parse_each_field(key: &str, value: &Value) -> Vec<Value> {
    match value {
        Value::List(items) => items.iter().map(|i| convert_single_field(key, i)).collect(),
        other => vec![convert_single_field(key, other)],
    }
}

fn sorted_fields(element: &Map) -> Vec<Value> {
    let mut keys: Vec<&String> = element.keys().collect();
    keys.sort();
    keys.into_iter()
        .flat_map(|k| parse_each_field(k, &element[k]))
        .collect()
}

fn elements<'v>(definition: &'v Map, key: &str) -> Result<&'v Vec<Value>, Fail> {
    match &definition[key] {
        Value::List(l) => Ok(l),
        other => Err(Fail::General(format!(
            "'{}' object has no attribute 'pop'",
            crate::cli::shorthand::py_type_name(other)
        ))),
    }
}

/// `definition_to_api_objects`.
fn definition_to_api_objects(definition: &Map) -> Result<Value, Fail> {
    if !definition.contains_key("objects") {
        return Err(definition_error("Missing \"objects\" key"));
    }
    let mut out = Vec::new();
    for element in elements(definition, "objects")? {
        let Value::Map(element) = element else {
            return Err(Fail::General(
                "'str' object has no attribute 'pop'".to_string(),
            ));
        };
        let mut element = element.clone();
        let Some(id) = element.shift_remove("id") else {
            return Err(definition_error(&format!(
                "Missing \"id\" key of element: {}",
                dumps(&Value::Map(element))
            )));
        };
        let name = element.shift_remove("name").unwrap_or_else(|| id.clone());
        let mut api = Map::new();
        api.insert("id".to_string(), id);
        api.insert("name".to_string(), name);
        api.insert("fields".to_string(), Value::List(sorted_fields(&element)));
        out.push(Value::Map(api));
    }
    Ok(Value::List(out))
}

/// `definition_to_api_parameters`.
fn definition_to_api_parameters(definition: &Map) -> Result<Option<Value>, Fail> {
    if !definition.contains_key("parameters") {
        return Ok(None);
    }
    let mut out = Vec::new();
    for element in elements(definition, "parameters")? {
        let Value::Map(element) = element else {
            return Err(Fail::General(
                "'str' object has no attribute 'pop'".to_string(),
            ));
        };
        let mut element = element.clone();
        let Some(id) = element.shift_remove("id") else {
            return Err(definition_error(&format!(
                "Missing \"id\" key of parameter: {}",
                dumps(&Value::Map(element))
            )));
        };
        let mut api = Map::new();
        api.insert("id".to_string(), id);
        api.insert(
            "attributes".to_string(),
            Value::List(sorted_fields(&element)),
        );
        out.push(Value::Map(api));
    }
    Ok(Some(Value::List(out)))
}

/// `definition_to_parameter_values`.
fn definition_to_parameter_values(definition: &Map) -> Result<Option<Value>, Fail> {
    let Some(values) = definition.get("values") else {
        return Ok(None);
    };
    let Value::Map(values) = values else {
        return Err(Fail::General(
            "string indices must be integers, not 'str'".to_string(),
        ));
    };
    let mut out = Vec::new();
    for (key, v) in values {
        let one = |s: &Value| {
            let mut m = Map::new();
            m.insert("id".to_string(), Value::str(key));
            m.insert("stringValue".to_string(), s.clone());
            Value::Map(m)
        };
        match v {
            Value::List(items) => out.extend(items.iter().map(one)),
            other => out.push(one(other)),
        }
    }
    Ok(Some(Value::List(out)))
}

/// `json.loads(value)` of a definition argument.
fn load_json(value: &Value) -> Result<Map, Fail> {
    let text = super::cloudwatch::value_str(value);
    match pyjson::loads(&text).map_err(Fail::Python)? {
        Value::Map(m) => Ok(m),
        other => Err(Fail::General(format!(
            "argument of type '{}' is not iterable",
            crate::cli::shorthand::py_type_name(&other)
        ))),
    }
}

struct Definition {
    put: bool,
}

fn custom(table: &mut ArgTable<'_>, name: &str, required: bool, nargs: Option<Nargs>) {
    table.insert(
        name.to_string(),
        Arg::custom_argument(
            name,
            CustomSpec {
                nargs,
                ..CustomSpec::default()
            },
            required,
        ),
    );
}

impl OperationHooks for Definition {
    /// `add_pipeline_definition` / `activate_pipeline_definition`.
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        if self.put {
            custom(table, "pipeline-definition", true, None);
            custom(table, "parameter-objects", false, None);
        }
        custom(table, "parameter-values-uri", false, None);
        custom(table, "parameter-values", false, Some(Nargs::OneOrMore));
        if self.put {
            table.shift_remove("pipeline-objects");
        }
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        let only_one = || {
            Fail::ParamValidation(
                "Only parameter-values or parameter-values-uri is allowed".to_string(),
            )
        };
        let has_values = |params: &Map| {
            params
                .get("parameterValues")
                .is_some_and(|v| !matches!(v, Value::Null))
        };
        match name {
            "pipeline-definition" => {
                let parsed = load_json(&value)?;
                let objects = definition_to_api_objects(&parsed)?;
                let parameter_objects = definition_to_api_parameters(&parsed)?;
                let parameter_values = definition_to_parameter_values(&parsed)?;
                params.insert("pipelineObjects".to_string(), objects);
                if !params.contains_key("parameterObjects")
                    && let Some(p) = parameter_objects
                {
                    params.insert("parameterObjects".to_string(), p);
                }
                if !params.contains_key("parameterValues")
                    && let Some(p) = parameter_values
                {
                    params.insert("parameterValues".to_string(), p);
                }
            }
            "parameter-objects" => {
                let parsed = load_json(&value)?;
                let p = definition_to_api_parameters(&parsed)?.unwrap_or(Value::Null);
                params.insert("parameterObjects".to_string(), p);
            }
            "parameter-values-uri" => {
                if has_values(params) {
                    return Err(only_one());
                }
                let parsed = load_json(&value)?;
                let p = definition_to_parameter_values(&parsed)?.unwrap_or(Value::Null);
                params.insert("parameterValues".to_string(), p);
            }
            "parameter-values" => {
                if has_values(params) {
                    return Err(only_one());
                }
                let items = match value {
                    Value::List(l) => l,
                    other => vec![other],
                };
                let mut object = Map::new();
                for item in &items {
                    let text = super::cloudwatch::value_str(item);
                    let Some((key, v)) = text.split_once('=') else {
                        return Err(Fail::ParamValidation(format!(
                            "Error in parameter: Invalid inline parameter format: {text}\n"
                        )));
                    };
                    let v = Value::str(v);
                    match object.get_mut(key) {
                        Some(Value::List(l)) => l.push(v),
                        Some(existing) => {
                            let first = existing.clone();
                            *existing = Value::List(vec![first, v]);
                        }
                        None => {
                            object.insert(key.to_string(), v);
                        }
                    }
                }
                let mut parsed = Map::new();
                parsed.insert("values".to_string(), Value::Map(object));
                let p = definition_to_parameter_values(&parsed)?.unwrap_or(Value::Null);
                params.insert("parameterValues".to_string(), p);
            }
            _ => {}
        }
        Ok(())
    }
}
