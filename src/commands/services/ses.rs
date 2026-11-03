//! `ses send-email` simple arguments (`awscli/customizations/sessendemail.py`).

use indexmap::IndexMap;

use super::exec;
use super::rds::rename_argument;
use crate::cli::argparse::{Nargs, NsVal};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "send-email" => exec::run_operation(ctx, "ses", name, args, &mut SendEmail),
        _ => Err(crate::commands::not_implemented()),
    }
}

struct SendEmail;

const ADDRESSES: &[(&str, &str)] = &[
    ("to", "ToAddresses"),
    ("cc", "CcAddresses"),
    ("bcc", "BccAddresses"),
];
const BODIES: &[(&str, &str)] = &[("subject", "Subject"), ("text", "Text"), ("html", "Html")];

/// `utils.validate_mutually_exclusive`.
pub(super) fn validate_mutually_exclusive(
    ns: &IndexMap<String, NsVal>,
    groups: &[&[&str]],
) -> Result<(), Fail> {
    let given: Vec<&String> = ns
        .iter()
        .filter(|(_, v)| !v.is_none())
        .map(|(k, _)| k)
        .collect();
    if !given
        .iter()
        .any(|k| groups.iter().any(|g| g.contains(&k.as_str())))
    {
        return Ok(());
    }
    let mut current: Option<&[&str]> = None;
    for key in given {
        let Some(group) = groups.iter().find(|g| g.contains(&key.as_str())) else {
            continue;
        };
        match current {
            None => current = Some(group),
            Some(c) if c != *group => {
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

impl OperationHooks for SendEmail {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        for name in ["message", "destination"] {
            if let Some(a) = table.get_mut(name) {
                a.required = false;
            }
        }
        rename_argument(table, "source", "from");
        for (name, _) in ADDRESSES {
            let spec = CustomSpec {
                nargs: Some(Nargs::OneOrMore),
                ..CustomSpec::default()
            };
            table.insert(name.to_string(), Arg::custom_argument(name, spec, false));
        }
        for (name, _) in BODIES {
            table.insert(
                name.to_string(),
                Arg::custom_argument(name, CustomSpec::default(), false),
            );
        }
    }

    fn args_parsed(
        &mut self,
        ns: &IndexMap<String, NsVal>,
        _paginate: &mut bool,
    ) -> Result<(), Fail> {
        validate_mutually_exclusive(ns, &[&["destination"], &["to", "cc", "bcc"]])?;
        validate_mutually_exclusive(ns, &[&["message"], &["text", "html"]])
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        let Some(value) = value.filter(Value::is_truthy) else {
            return Ok(());
        };
        if let Some((_, key)) = ADDRESSES.iter().find(|(n, _)| *n == name) {
            let dest = params
                .entry("Destination".to_string())
                .or_insert_with(Value::map);
            if let Value::Map(d) = dest {
                d.insert(key.to_string(), value);
            }
        } else if let Some((_, key)) = BODIES.iter().find(|(n, _)| *n == name) {
            let message = params.entry("Message".to_string()).or_insert_with(|| {
                let mut m = Map::new();
                m.insert("Subject".to_string(), Value::map());
                m.insert("Body".to_string(), Value::map());
                Value::Map(m)
            });
            let Value::Map(message) = message else {
                return Ok(());
            };
            let mut data = Map::new();
            data.insert("Data".to_string(), value);
            if *key == "Subject" {
                message.insert("Subject".to_string(), Value::Map(data));
            } else if let Some(Value::Map(body)) = message.get_mut("Body") {
                body.insert(key.to_string(), Value::Map(data));
            }
        }
        Ok(())
    }
}
