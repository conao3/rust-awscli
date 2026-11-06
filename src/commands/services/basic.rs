//! `BasicCommand` (`awscli/customizations/commands.py`): the argument
//! table of a command a customization adds, its parsing
//! (`ArgTableArgParser`), and the per-argument processing of
//! `BasicCommand.__call__` (`load-cli-arg`, `process-cli-arg` /
//! `unpack_cli_arg`, `validate_parameters`).

use indexmap::IndexMap;
use serde_json::{Value as Json, json};

use crate::cli::argparse::{Kind, Nargs, NsVal, Parsed as ArgParsed, Parser};
use crate::cli::argprocess::{ArgContext, load_cli_arg, unpack_argument};
use crate::cli::render::argparse_error;
use crate::cli::{
    Arg, ArgTable, CustomSpec, Fail, custom_action, format_unknown_options_error,
    option_suggestion_candidates, validate,
};
use crate::driver::{Context, write_stdout};
use crate::error::CliError;
use crate::model::ServiceModel;
use crate::value::Value;

/// One entry of `ARG_TABLE`.
#[derive(Debug, Clone)]
pub(crate) struct ArgDef {
    pub name: &'static str,
    pub spec: CustomSpec,
    pub required: bool,
    /// `cli_type_name` (the scalar argument model when there is no schema
    /// and no `nargs`).
    pub type_name: Option<&'static str>,
    /// `schema` (`create_argument_model_from_schema`).
    pub schema: Option<Json>,
}

impl ArgDef {
    pub(crate) fn new(name: &'static str) -> ArgDef {
        ArgDef {
            name,
            spec: CustomSpec::default(),
            required: false,
            type_name: None,
            schema: None,
        }
    }

    pub(crate) fn required(mut self) -> ArgDef {
        self.required = true;
        self
    }

    /// `'action': 'store_true'`.
    pub(crate) fn flag(mut self) -> ArgDef {
        self.spec.action = Some(Kind::StoreTrue);
        self
    }

    /// `'action': 'store_false'`.
    pub(crate) fn store_false(mut self) -> ArgDef {
        self.spec.action = Some(Kind::StoreFalse);
        self
    }

    /// `'action': 'append'`.
    pub(crate) fn append(mut self) -> ArgDef {
        self.spec.action = Some(Kind::Append);
        self
    }

    pub(crate) fn nargs(mut self, nargs: Nargs) -> ArgDef {
        self.spec.nargs = Some(nargs);
        self
    }

    pub(crate) fn positional(mut self) -> ArgDef {
        self.spec.positional = true;
        self
    }

    pub(crate) fn choices(mut self, choices: &[&str]) -> ArgDef {
        self.spec.choices = choices.iter().map(|c| c.to_string()).collect();
        self
    }

    pub(crate) fn default(mut self, value: NsVal) -> ArgDef {
        self.spec.default = Some(value);
        self
    }

    pub(crate) fn default_str(self, value: &str) -> ArgDef {
        self.default(NsVal::Str(value.to_string()))
    }

    pub(crate) fn dest(mut self, dest: &str) -> ArgDef {
        self.spec.dest = Some(dest.to_string());
        self
    }

    pub(crate) fn const_(mut self, value: &str) -> ArgDef {
        self.spec.const_ = Some(value.to_string());
        self
    }

    /// `'cli_type_name': name`.
    pub(crate) fn type_name(mut self, name: &'static str) -> ArgDef {
        self.type_name = Some(name);
        self
    }

    /// `'cli_type_name': 'integer'`.
    pub(crate) fn int(self) -> ArgDef {
        self.type_name("integer")
    }

    /// `'schema': schema`.
    pub(crate) fn schema(mut self, schema: Json) -> ArgDef {
        self.schema = Some(schema);
        self
    }

    /// The keyword arguments `add_to_parser` passes (`nargs='+'` for a
    /// list argument model).
    fn effective_spec(&self) -> CustomSpec {
        let mut spec = self.spec.clone();
        if self
            .schema
            .as_ref()
            .is_some_and(|s| s["type"].as_str() == Some("array"))
        {
            spec.nargs = Some(Nargs::OneOrMore);
        }
        spec
    }

    /// The key in the parsed namespace.
    pub(crate) fn dest_name(&self) -> String {
        match &self.spec.dest {
            Some(d) => d.clone(),
            None if self.spec.positional => self.name.to_string(),
            None => self.name.replace('-', "_"),
        }
    }

    fn cli_name(&self) -> String {
        if self.spec.positional {
            self.name.to_string()
        } else {
            format!("--{}", self.name)
        }
    }

    /// `CustomArgument.argument_model`: the schema's model, or the scalar
    /// model of `cli_type_name` when there is no `nargs`.
    fn argument_model(&self) -> Option<ServiceModel> {
        if let Some(schema) = &self.schema {
            return Some(ServiceModel::from_shapes(
                "custom",
                transform_schema(schema),
            ));
        }
        if self.spec.nargs.is_some() {
            return None;
        }
        let type_name = match (self.type_name, self.spec.action) {
            (Some(t), _) => t,
            (None, Some(Kind::StoreTrue | Kind::StoreFalse)) => "boolean",
            _ => "string",
        };
        Some(ServiceModel::from_shapes(
            "custom",
            transform_schema(&json!({ "type": type_name })),
        ))
    }
}

/// `SchemaTransformer.transform` (`awscli/schema.py`): the shapes, with the
/// argument's shape named `InputShape`.
pub(crate) fn transform_schema(schema: &Json) -> Json {
    struct Namer(IndexMap<String, usize>);
    impl Namer {
        fn name(&mut self, type_name: &str) -> String {
            let n = self.0.entry(type_name.to_string()).or_insert(0);
            *n += 1;
            let mut chars = type_name.chars();
            let cap: String = chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect())
                .unwrap_or_default();
            format!("{cap}Type{n}")
        }
    }
    fn aws_type(schema: &Json) -> String {
        let t = schema["type"].as_str().unwrap_or_default();
        match t {
            "object" => "structure",
            "array" => "list",
            other => other,
        }
        .to_string()
    }
    fn initial(schema: &Json) -> serde_json::Map<String, Json> {
        let mut shape = serde_json::Map::new();
        shape.insert("type".to_string(), Json::String(aws_type(schema)));
        if let Some(d) = schema.get("description") {
            shape.insert("documentation".to_string(), d.clone());
        }
        if let Some(e) = schema.get("enum") {
            shape.insert("enum".to_string(), e.clone());
        }
        shape
    }
    fn transform(
        schema: &Json,
        shapes: &mut serde_json::Map<String, Json>,
        name: &str,
        namer: &mut Namer,
    ) {
        let mut shape = initial(schema);
        match schema["type"].as_str() {
            Some("object") => {
                let mut members = serde_json::Map::new();
                let mut required = Vec::new();
                if let Some(props) = schema["properties"].as_object() {
                    for (key, value) in props {
                        let member_name = namer.name(&aws_type(value));
                        members.insert(key.clone(), json!({ "shape": member_name }));
                        if value.get("required").and_then(Json::as_bool) == Some(true) {
                            required.push(Json::String(key.clone()));
                        }
                        transform(value, shapes, &member_name, namer);
                    }
                }
                shape.insert("members".to_string(), Json::Object(members));
                if !required.is_empty() {
                    shape.insert("required".to_string(), Json::Array(required));
                }
            }
            Some("array") => {
                let member_name = namer.name(&aws_type(&schema["items"]));
                shape.insert("member".to_string(), json!({ "shape": member_name }));
                transform(&schema["items"], shapes, &member_name, namer);
            }
            Some("map") => {
                for attribute in ["key", "value"] {
                    let n = namer.name(&aws_type(&schema[attribute]));
                    shape.insert(attribute.to_string(), json!({ "shape": n }));
                    transform(&schema[attribute], shapes, &n, namer);
                }
            }
            _ => {}
        }
        shapes.insert(name.to_string(), Json::Object(shape));
    }
    let mut shapes = serde_json::Map::new();
    transform(
        schema,
        &mut shapes,
        "InputShape",
        &mut Namer(IndexMap::new()),
    );
    Json::Object(shapes)
}

/// The processed arguments of a `BasicCommand` (`parsed_args`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Args {
    pub values: IndexMap<String, Value>,
}

impl Args {
    /// The value of the argument whose namespace key is `key` (`dest`, or
    /// the name with `_` for `-`); `None` for a missing or `None` value.
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        let key = key.replace('-', "_");
        self.values.get(&key).filter(|v| !matches!(v, Value::Null))
    }

    pub(crate) fn str(&self, key: &str) -> Option<String> {
        self.get(key).map(|v| match v {
            Value::Str(s) => s.clone(),
            other => crate::pyfmt::py_str(other),
        })
    }

    pub(crate) fn bool(&self, key: &str) -> bool {
        self.get(key).is_some_and(Value::is_truthy)
    }

    pub(crate) fn list(&self, key: &str) -> Option<Vec<Value>> {
        match self.get(key)? {
            Value::List(l) => Some(l.clone()),
            other => Some(vec![other.clone()]),
        }
    }

    pub(crate) fn str_list(&self, key: &str) -> Option<Vec<String>> {
        self.list(key).map(|l| {
            l.iter()
                .map(|v| match v {
                    Value::Str(s) => s.clone(),
                    other => crate::pyfmt::py_str(other),
                })
                .collect()
        })
    }

    pub(crate) fn i64(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(Value::as_i64)
    }
}

fn ns_value(v: &NsVal) -> Value {
    match v {
        NsVal::None | NsVal::NotSpecified => Value::Null,
        NsVal::Bool(b) => Value::Bool(*b),
        NsVal::Str(s) => Value::Str(s.clone()),
        NsVal::Int(i) => Value::Int(i.clone()),
        NsVal::Float(f) => Value::Float(*f),
        NsVal::List(l) => Value::List(l.iter().map(ns_value).collect()),
    }
}

/// The `ArgTable` of `defs` (for the option suggestions).
fn arg_table(defs: &[ArgDef]) -> ArgTable<'static> {
    defs.iter()
        .map(|d| {
            (
                d.name.to_string(),
                Arg::custom_argument(d.name, d.effective_spec(), d.required),
            )
        })
        .collect()
}

/// `BasicCommand.__call__` up to `_run_main` for a command without
/// subcommands: `None` when `help` was requested (the help text is
/// written), else the processed arguments. `lineage` is the command path
/// (`["ecr", "get-login-password"]`).
pub(crate) fn parse(
    ctx: &Context,
    lineage: &[&str],
    defs: &[ArgDef],
    args: &[String],
) -> Result<Option<Args>, CliError> {
    parse_args(lineage, defs, args)
        .map_err(|f| crate::cli::render_fail(f, &ctx.globals, &ctx.session))
        .and_then(|parsed| match parsed {
            Some(a) => {
                if let Some((first, rest)) = lineage.split_first() {
                    ctx.session.set_command(first, &rest.join("."));
                }
                Ok(Some(a))
            }
            None => {
                write_stdout(ctx, help_text(lineage, defs).as_bytes())?;
                Ok(None)
            }
        })
}

/// The help stand-in (the command name and its options), like the one of
/// model operations.
fn help_text(lineage: &[&str], defs: &[ArgDef]) -> String {
    let mut out = format!(
        "{}\n\nOptions:\n\n",
        lineage.last().copied().unwrap_or_default()
    );
    for d in defs {
        out.push_str(&format!("  {}\n", d.cli_name()));
    }
    out
}

/// [`parse`] without rendering: `Ok(None)` for `help`.
pub(crate) fn parse_args(
    lineage: &[&str],
    defs: &[ArgDef],
    args: &[String],
) -> Result<Option<Args>, Fail> {
    if args.len() == 1 && args[0] == "help" {
        return Ok(None);
    }
    let mut parser = Parser::new();
    for d in defs {
        parser.add(custom_action(d.name, &d.effective_spec(), d.required));
    }
    let (ns, remaining) = match parser
        .parse_known_args(args)
        .map_err(|e| argparse_error(&e.to_string()))?
    {
        ArgParsed::Args { ns, extras } => (ns, extras),
        ArgParsed::Version => unreachable!("no version action"),
    };
    let command = lineage.last().copied().unwrap_or_default();
    let models: Vec<Option<ServiceModel>> = defs.iter().map(ArgDef::argument_model).collect();
    let mut values = IndexMap::new();
    for (key, raw) in &ns {
        let xformed = key.replace('_', "-");
        let def = defs.iter().position(|d| d.name == xformed);
        let mut value = ns_value(raw);
        let cli_name = def.map(|i| defs[i].cli_name()).unwrap_or_default();
        if let Some(loaded) = load_cli_arg(&cli_name, &value)? {
            value = loaded;
        }
        if let Some(i) = def
            && let Some(model) = &models[i]
            && !matches!(value, Value::Null)
        {
            let shape = model.shape("InputShape").expect("InputShape");
            value = unpack_argument(
                &ArgContext {
                    name: defs[i].name,
                    cli_name: &cli_name,
                    shape: &shape,
                    service: "custom",
                    operation: command,
                },
                &value,
            )?;
            if let Some(report) = validate::validate(&value, &shape) {
                return Err(Fail::ParamValidation(report));
            }
        }
        values.insert(key.clone(), value);
    }
    if !remaining.is_empty() {
        return Err(Fail::ParamValidation(format_unknown_options_error(
            &remaining,
            &option_suggestion_candidates(Some(&arg_table(defs))),
        )));
    }
    Ok(Some(Args { values }))
}

/// `sys.stdout.write(text)` (not through the pager).
pub(crate) fn print(text: &str) {
    print_bytes(text.as_bytes());
}

pub(crate) fn print_bytes(bytes: &[u8]) {
    crate::output::stdout::write(bytes);
}

/// `sys.stdout.flush()`.
pub(crate) fn flush() {
    crate::output::stdout::flush();
}

/// `sys.stderr.write(text)`.
pub(crate) fn eprint(text: &str) {
    use std::io::Write as _;
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
}

/// `ParamValidationError(message)` raised by a command (exit 252).
pub(crate) fn param_validation(ctx: &Context, message: &str) -> CliError {
    crate::cli::render_fail(
        Fail::ParamValidation(message.to_string()),
        &ctx.globals,
        &ctx.session,
    )
}

/// The `usage: ... too few arguments` error of `_raise_usage_error`.
pub(crate) fn usage_error(ctx: &Context, lineage: &[&str]) -> CliError {
    param_validation(
        ctx,
        &format!(
            "usage: awsr [options] {} <subcommand> [parameters]\nawsr: [ERROR]: too few arguments",
            lineage.join(" ")
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn scalar_and_list_args() {
        let defs = vec![
            ArgDef::new("name").required(),
            ArgDef::new("count").int(),
            ArgDef::new("verbose").flag(),
            ArgDef::new("paths").nargs(Nargs::OneOrMore),
        ];
        let a = parse_args(
            &["svc", "cmd"],
            &defs,
            &s(&["--name", "x", "--count", "3", "--paths", "a", "b"]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(a.str("name").as_deref(), Some("x"));
        assert_eq!(a.i64("count"), Some(3));
        assert!(!a.bool("verbose"));
        assert_eq!(a.str_list("paths"), Some(s(&["a", "b"])));
    }

    #[test]
    fn errors() {
        let defs = vec![ArgDef::new("name").required(), ArgDef::new("count").int()];
        let e = parse_args(&["svc", "cmd"], &defs, &s(&[])).unwrap_err();
        assert_eq!(
            e,
            Fail::ParamValidation(format!(
                "the following arguments are required: --name\n\n{}",
                crate::cli::render::format_usage()
            ))
        );
        let e = parse_args(&["svc", "cmd"], &defs, &s(&["--name", "x", "--nmae"])).unwrap_err();
        assert!(matches!(e, Fail::ParamValidation(m) if m.starts_with("Unknown options: --nmae")));
        let e =
            parse_args(&["svc", "cmd"], &defs, &s(&["--name", "x", "--count", "z"])).unwrap_err();
        assert_eq!(
            e,
            Fail::Python("invalid literal for int() with base 10: 'z'".to_string())
        );
    }

    #[test]
    fn schema_shorthand() {
        let defs = vec![ArgDef::new("tags").schema(json!({
            "type": "array",
            "items": {"type": "object", "properties": {
                "Key": {"type": "string"}, "Value": {"type": "string"}
            }}
        }))];
        let a = parse_args(
            &["svc", "cmd"],
            &defs,
            &s(&["--tags", "Key=a,Value=b", "Key=c,Value=d"]),
        )
        .unwrap()
        .unwrap();
        let tags = a.list("tags").unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].get("Key"), Some(&Value::str("a")));
    }
}
