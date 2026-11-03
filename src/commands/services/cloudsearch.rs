//! `cloudsearch define-expression` / `define-index-field` flattened
//! arguments (`awscli/customizations/cloudsearch.py`,
//! `awscli/customizations/flatten.py`).

use super::exec;
use crate::cli::shorthand::{float_of, int_of};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::pyfmt::py_str;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    let config: &'static [Flat] = match name {
        "define-expression" => DEFINE_EXPRESSION,
        "define-index-field" => DEFINE_INDEX_FIELD,
        _ => return Err(crate::commands::not_implemented()),
    };
    let container = match name {
        "define-expression" => "expression",
        _ => "index-field",
    };
    exec::run_operation(
        ctx,
        "cloudsearch",
        name,
        args,
        &mut Flatten {
            container,
            config,
            args: Vec::new(),
            container_shape: String::new(),
        },
    )
}

/// One `flatten` entry: (property, CLI name, `type` override, `hydrate`).
struct Flat {
    prop: &'static str,
    name: &'static str,
    type_name: Option<&'static str>,
    hydrate: bool,
}

const fn flat(prop: &'static str, name: &'static str) -> Flat {
    Flat {
        prop,
        name,
        type_name: None,
        hydrate: false,
    }
}

const fn hydrated(prop: &'static str, name: &'static str, type_name: Option<&'static str>) -> Flat {
    Flat {
        prop,
        name,
        type_name,
        hydrate: true,
    }
}

const DEFINE_EXPRESSION: &[Flat] = &[
    flat("ExpressionName", "name"),
    flat("ExpressionValue", "expression"),
];

const DEFINE_INDEX_FIELD: &[Flat] = &[
    flat("IndexFieldName", "name"),
    flat("IndexFieldType", "type"),
    hydrated("IntOptions.DefaultValue", "default-value", Some("string")),
    hydrated("IntOptions.FacetEnabled", "facet-enabled", None),
    hydrated("IntOptions.SearchEnabled", "search-enabled", None),
    hydrated("IntOptions.ReturnEnabled", "return-enabled", None),
    hydrated("IntOptions.SortEnabled", "sort-enabled", None),
    hydrated("IntOptions.SourceField", "source-field", Some("string")),
    hydrated("TextOptions.HighlightEnabled", "highlight-enabled", None),
    hydrated("TextOptions.AnalysisScheme", "analysis-scheme", None),
];

/// `FlattenArguments.flatten_args` and `FlattenedArgument.add_to_params`.
struct Flatten {
    container: &'static str,
    config: &'static [Flat],
    /// (CLI name, property, type, hydrate) of the flattened arguments.
    args: Vec<(&'static str, &'static str, String, bool)>,
    /// `argument_model.name` of the container argument.
    container_shape: String,
}

impl OperationHooks for Flatten {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        let Some(container) = table.get(self.container).cloned() else {
            return;
        };
        let shape = container.shape.expect("model argument");
        self.container_shape = shape.name.to_string();
        let mut overwritten = false;
        for f in self.config {
            let mut owner = Some(shape);
            let pieces: Vec<&str> = f.prop.split('.').collect();
            for piece in &pieces[..pieces.len() - 1] {
                owner = owner.and_then(|s| s.member_shape(piece));
            }
            let last = pieces[pieces.len() - 1];
            let member = owner.and_then(|s| s.member_shape(last));
            let required = owner.is_some_and(|s| s.required().contains(&last));
            let type_name = f
                .type_name
                .map(str::to_string)
                .or_else(|| member.map(|m| m.type_name().to_string()))
                .unwrap_or_default();
            table.insert(
                f.name.to_string(),
                Arg::custom_argument(f.name, CustomSpec::default(), required),
            );
            self.args.push((f.name, f.prop, type_name, f.hydrate));
            if f.name == self.container {
                overwritten = true;
            }
        }
        if !overwritten {
            table.shift_remove(self.container);
        }
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value else { return Ok(()) };
        let Some((_, prop, type_name, hydrate)) = self.args.iter().find(|a| a.0 == name) else {
            return Ok(());
        };
        let text = match &value {
            Value::Str(s) => s.clone(),
            other => py_str(other),
        };
        let value = match type_name.as_str() {
            "boolean" => Value::Bool(text.to_lowercase() != "false"),
            "integer" | "long" => int_of(&value).map_err(Fail::Python)?,
            "float" | "double" => float_of(&value).map_err(Fail::Python)?,
            _ => value,
        };
        let container = params
            .entry(self.container_shape.clone())
            .or_insert_with(Value::map);
        let Value::Map(container) = container else {
            return Ok(());
        };
        if *hydrate {
            index_hydrate(container, prop, value)
        } else {
            container.insert(prop.to_string(), value);
            Ok(())
        }
    }
}

/// `index_hydrate` (with the `IndexField` container).
fn index_hydrate(index_field: &mut Map, key: &str, value: Value) -> Result<(), Fail> {
    let Some(field_type) = index_field.get("IndexFieldType") else {
        return Err(Fail::ParamValidation(
            "You must pass the --type option.".to_string(),
        ));
    };
    let mut t: String = py_str(field_type)
        .split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => c
                    .to_uppercase()
                    .chain(chars.flat_map(char::to_lowercase))
                    .collect(),
                None => String::new(),
            }
        })
        .collect();
    if t == "Latlon" {
        t = "LatLon".to_string();
    }
    let last = key.rsplit('.').next().unwrap_or(key);
    let mut value = value;
    if last == "DefaultValue" {
        value = match t.as_str() {
            "Int" | "IntArray" => int_of(&value).map_err(Fail::Python)?,
            "Double" | "DoubleArray" => float_of(&value).map_err(Fail::Python)?,
            _ => value,
        };
    }
    let options = index_field
        .entry(format!("{t}Options"))
        .or_insert_with(Value::map);
    if let Value::Map(o) = options {
        o.insert(last.to_string(), value);
    }
    Ok(())
}
