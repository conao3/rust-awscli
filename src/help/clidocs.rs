//! The help document builders: `awscli/bcdoc/docevents.py`
//! `generate_events`, the `awscli/clidocs.py` event handlers,
//! `awscli/customizations/commands.py` `BasicDocHandler`,
//! `awscli/customizations/waiters.py` `WaiterCommandDocHandler`, the
//! `argprocess.ParamShorthandDocGen` examples, and the customizations that
//! write into the documents (`paginate.add_paging_description`,
//! `addexamples.add_examples`, `iot_data`, `ec2.secgroupsimplify`,
//! `route53`, `datapipeline`, `s3events`).

use indexmap::IndexMap;
use serde_json::{Map, Value as Json};

use crate::bjson::Node;

use super::data;
use super::restdoc::{ReSTDocument, py_readlines};
use crate::model::{Operation, Shape};

/// `awscli.SCALAR_TYPES`.
fn is_scalar(t: &str) -> bool {
    matches!(
        t,
        "string" | "float" | "integer" | "long" | "boolean" | "double" | "blob" | "timestamp"
    )
}

/// One entry of a help command's `arg_table`.
#[derive(Clone)]
pub(crate) struct HelpArg<'a> {
    pub name: String,
    pub cli_name: String,
    pub documentation: String,
    pub cli_type_name: String,
    pub required: bool,
    pub document_as_required: bool,
    pub group_name: Option<String>,
    pub positional_arg: bool,
    pub nargs: Option<String>,
    pub synopsis: Option<String>,
    pub undocumented: bool,
    pub model: Option<Shape<'a>>,
}

/// The help command classes and their document event handlers.
pub(crate) enum Kind<'a> {
    /// `ProviderHelpCommand` / `ProviderDocumentEventHandler`.
    Provider {
        description: String,
        synopsis: String,
        usage: String,
    },
    /// `ServiceHelpCommand` / `ServiceDocumentEventHandler`.
    Service { documentation: String },
    /// `OperationHelpCommand` / `OperationDocumentEventHandler`.
    Operation {
        op: Operation<'a>,
        documentation: String,
        output: Option<Shape<'a>>,
    },
    /// `TopicListerCommand` / `TopicListerDocumentEventHandler`.
    TopicLister,
    /// `TopicHelpCommand` / `TopicDocumentEventHandler`.
    Topic,
    /// `BasicHelp` / `BasicDocHandler` (`WaiterCommandDocHandler` when
    /// `waiter`).
    Basic {
        description: String,
        synopsis: String,
        examples: String,
        waiter: bool,
    },
}

/// A help command with what its document handlers read.
pub(crate) struct Page<'a> {
    pub name: String,
    pub event_class: String,
    pub kind: Kind<'a>,
    pub args: Vec<HelpArg<'a>>,
    /// The documented command table names.
    pub commands: Vec<String>,
    /// Whether the command table is non-empty (including undocumented
    /// commands).
    pub has_commands: bool,
    pub related_items: Vec<String>,
    doc: ReSTDocument,
    arg_groups: IndexMap<String, Vec<String>>,
    documented_arg_groups: Vec<String>,
}

const TOPIC_DESCRIPTION: &str = "This is the AWS CLI Topic Guide. It gives access to a set of topics that provide a deeper understanding of the CLI. To access the list of topics from the command line, run ``aws help topics``. To access a specific topic from the command line, run ``aws help [topicname]``, where ``topicname`` is the name of the topic as it appears in the output from ``aws help topics``.";

const VALID_TAGS: &[&str] = &[
    "category",
    "description",
    "title",
    "related topic",
    "related command",
];

/// `TopicTagDB` over `topics/topic-tags.json`.
pub(crate) struct TopicTagDb {
    tags: Map<String, Json>,
}

impl TopicTagDb {
    pub(crate) fn load() -> TopicTagDb {
        let index = data::topic_file("topic-tags.json").expect("topic-tags.json");
        let tags: Json = serde_json::from_str(&index).expect("topic-tags.json");
        TopicTagDb {
            tags: tags.as_object().cloned().unwrap_or_default(),
        }
    }

    pub(crate) fn topic_names(&self) -> Vec<String> {
        self.tags.keys().cloned().collect()
    }

    fn single_value(&self, topic: &str, tag: &str) -> String {
        self.tags
            .get(topic)
            .and_then(|t| t.get(tag))
            .and_then(|v| v.get(0))
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// `TopicTagDB.query`.
    fn query(&self, tag: &str) -> IndexMap<String, Vec<String>> {
        let mut out: IndexMap<String, Vec<String>> = IndexMap::new();
        for (topic, tags) in &self.tags {
            if let Some(values) = tags.get(tag).and_then(Json::as_array) {
                for v in values.iter().filter_map(Json::as_str) {
                    out.entry(v.to_string()).or_default().push(topic.clone());
                }
            }
        }
        out
    }
}

/// `TopicDocumentEventHandler._remove_tags_from_content`.
fn remove_tags_from_content(contents: &str) -> String {
    let lines = py_readlines(contents);
    let mut begin = 0;
    for (i, line) in lines.iter().enumerate() {
        if !VALID_TAGS
            .iter()
            .any(|t| line.starts_with(&format!(":{t}:")))
        {
            begin = i;
            break;
        }
    }
    lines[begin..].concat()
}

fn shape_doc(shape: &Shape<'_>) -> String {
    shape
        .get_str("documentation")
        .unwrap_or_default()
        .to_string()
}

fn is_string(shape: &Shape<'_>) -> bool {
    shape.type_name() == "string"
}

fn enum_values<'a>(shape: &Shape<'a>) -> Vec<&'a str> {
    shape
        .get("enum")
        .and_then(Node::as_array)
        .map(|a| a.iter().filter_map(Node::as_str).collect())
        .unwrap_or_default()
}

/// `utils.is_document_type`.
fn is_document_type(shape: &Shape<'_>) -> bool {
    shape.type_name() == "structure" && shape.get_bool("document")
}

/// `utils.is_tagged_union_type`.
fn is_tagged_union(shape: &Shape<'_>) -> bool {
    shape.type_name() == "structure" && shape.get_bool("union")
}

/// `utils.is_streaming_blob_type`.
fn is_streaming_blob(shape: &Shape<'_>) -> bool {
    shape.type_name() == "blob" && shape.get_bool("streaming")
}

/// `botocore.utils.is_json_value_header`.
fn is_json_value_header(shape: &Shape<'_>) -> bool {
    shape.get_bool("jsonvalue")
        && shape.get_str("location") == Some("header")
        && shape.type_name() == "string"
}

/// `CLIDocumentEventHandler._get_argument_type_name`.
fn argument_type_name(shape: Option<&Shape<'_>>, default: &str) -> String {
    if let Some(s) = shape {
        if is_json_value_header(s) {
            return "JSON".to_string();
        }
        if is_document_type(s) {
            return "document".to_string();
        }
        if is_streaming_blob(s) {
            return "streaming blob".to_string();
        }
        if is_tagged_union(s) {
            return "tagged union structure".to_string();
        }
    }
    default.to_string()
}

/// `utils.ShapeWalker` with a recording visitor: the shapes visited.
fn walk_shapes<'a>(shape: &Shape<'a>, stack: &mut Vec<&'a str>, visited: &mut Vec<Shape<'a>>) {
    if stack.contains(&shape.name) {
        return;
    }
    stack.push(shape.name);
    visited.push(*shape);
    match shape.type_name() {
        "structure" => {
            for (_, m) in shape.members() {
                walk_shapes(&m, stack, visited);
            }
        }
        "list" => {
            if let Some(m) = shape.list_member() {
                walk_shapes(&m, stack, visited);
            }
        }
        "map" => {
            if let Some(v) = shape.map_value() {
                walk_shapes(&v, stack, visited);
            }
        }
        _ => {}
    }
    stack.pop();
}

/// `utils.is_document_type_container`.
fn is_document_type_container(shape: &Shape<'_>) -> bool {
    let mut visited = Vec::new();
    walk_shapes(shape, &mut Vec::new(), &mut visited);
    let Some(end) = visited.pop() else {
        return false;
    };
    if !is_document_type(&end) {
        return false;
    }
    visited
        .iter()
        .all(|s| matches!(s.type_name(), "list" | "map"))
}

/// `argprocess._supports_shorthand_syntax`.
fn supports_shorthand(shape: &Shape<'_>) -> bool {
    if is_document_type_container(shape) {
        return false;
    }
    match shape.type_name() {
        "structure" | "map" => true,
        "list" => shape
            .list_member()
            .is_some_and(|m| matches!(m.type_name(), "structure" | "list" | "map")),
        _ => false,
    }
}

/// Python's `str()` of a JSON scalar from a model.
fn py_str(v: &Node) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(b) = v.as_bool() {
        return if b { "True" } else { "False" }.to_string();
    }
    if v.is_null() {
        return "None".to_string();
    }
    match v.as_number_text() {
        Some(text) if v.is_i64() || v.is_u64() => text.to_string(),
        Some(_) => crate::pyfmt::float_repr(v.as_f64().unwrap_or_default()),
        None => v.to_string(),
    }
}

struct TooComplex;

/// `ParamShorthandDocGen`.
mod shorthand {
    use super::*;

    const MAX_STACK: usize = 3;

    pub(super) enum Example {
        DontDoc,
        Text(String),
    }

    fn uses_old_list_case(service: &str, operation: &str, arg: &str) -> bool {
        let cases: &[(&str, &str, &str)] = &[
            ("firehose", "put-record-batch", "records"),
            (
                "workspaces",
                "reboot-workspaces",
                "reboot-workspace-requests",
            ),
            (
                "workspaces",
                "rebuild-workspaces",
                "rebuild-workspace-requests",
            ),
            (
                "workspaces",
                "terminate-workspaces",
                "terminate-workspace-requests",
            ),
            ("elb", "remove-tags", "tags"),
            ("elb", "describe-instance-health", "instances"),
            (
                "elb",
                "deregister-instances-from-load-balancer",
                "instances",
            ),
            ("elb", "register-instances-with-load-balancer", "instances"),
        ];
        cases
            .iter()
            .any(|(s, o, a)| *s == service && *o == operation && *a == arg)
    }

    /// `generate_shorthand_example`.
    pub(super) fn generate(
        arg: &HelpArg<'_>,
        model: &Shape<'_>,
        service: &str,
        operation: &str,
    ) -> Example {
        if model.type_name() == "list"
            && let Some(member) = model.list_member()
            && member.type_name() == "structure"
            && member.members().len() == 1
            && uses_old_list_case(service, operation, &arg.name)
        {
            let member_name = member.members()[0].0;
            if model.get("min").and_then(Node::as_i64) == Some(1)
                && model.get("max").and_then(Node::as_i64) == Some(1)
            {
                return Example::Text(format!("{} {member_name}1", arg.cli_name));
            }
            return Example::Text(format!(
                "{} {member_name}1 {member_name}2 {member_name}3",
                arg.cli_name
            ));
        }
        if model.type_name() == "structure" {
            let members = model.members();
            if members.len() == 1
                && let Some(v) = model.member_shape("Value")
                && v.type_name() == "string"
            {
                return Example::DontDoc;
            }
        }
        let mut stack = Vec::new();
        let r = if model.type_name() == "list" {
            let member = model.list_member().expect("list member");
            docs(&member, &mut stack).map(|s| s + " ...")
        } else {
            docs(model, &mut stack)
        };
        Example::Text(r.unwrap_or_default())
    }

    fn docs<'a>(shape: &Shape<'a>, stack: &mut Vec<&'a str>) -> Result<String, TooComplex> {
        if stack.len() > MAX_STACK {
            return Err(TooComplex);
        }
        match shape.type_name() {
            "structure" => structure_docs(shape, stack),
            "list" => list_docs(shape, stack),
            "map" => map_docs(shape, stack),
            t => Ok(t.to_string()),
        }
    }

    fn list_docs<'a>(shape: &Shape<'a>, stack: &mut Vec<&'a str>) -> Result<String, TooComplex> {
        let member = shape.list_member().expect("list member");
        stack.push(member.name);
        let element = docs(&member, stack);
        stack.pop();
        let element = element?;
        if matches!(member.type_name(), "structure" | "map" | "list") || stack.len() > 1 {
            Ok(format!("[{element},{element}]"))
        } else {
            Ok(format!("{element},{element}"))
        }
    }

    fn map_docs<'a>(shape: &Shape<'a>, stack: &mut Vec<&'a str>) -> Result<String, TooComplex> {
        let key = shape.map_key().expect("map key");
        let value = shape.map_value().expect("map value");
        stack.push(value.name);
        let value_docs = docs(&value, stack);
        stack.pop();
        let value_docs = value_docs?;
        let mut start = format!("KeyName1={value_docs},KeyName2={value_docs}");
        let key_enum = enum_values(&key);
        if !key_enum.is_empty() && stack.is_empty() {
            start.push_str("\n\nWhere valid key names are:\n");
            for e in key_enum {
                start.push_str(&format!("  {e}\n"));
            }
        } else if !stack.is_empty() {
            start = format!("{{{start}}}");
        }
        Ok(start)
    }

    fn structure_docs<'a>(
        shape: &Shape<'a>,
        stack: &mut Vec<&'a str>,
    ) -> Result<String, TooComplex> {
        let mut parts = Vec::new();
        for (name, member) in shape.members() {
            if is_document_type_container(&member) {
                continue;
            }
            parts.push(member_docs(name, &member, stack)?);
        }
        let inner = parts.join(",");
        if stack.is_empty() {
            Ok(inner)
        } else {
            Ok(format!("{{{inner}}}"))
        }
    }

    fn member_docs<'a>(
        name: &str,
        shape: &Shape<'a>,
        stack: &mut Vec<&'a str>,
    ) -> Result<String, TooComplex> {
        if stack.contains(&shape.name) {
            return Ok("( ... recursive ... )".to_string());
        }
        stack.push(shape.name);
        let value = docs(shape, stack);
        stack.pop();
        Ok(format!("{name}={}", value?))
    }
}

impl<'a> Page<'a> {
    pub(crate) fn new(
        name: &str,
        event_class: &str,
        kind: Kind<'a>,
        args: Vec<HelpArg<'a>>,
    ) -> Page<'a> {
        let mut arg_groups: IndexMap<String, Vec<String>> = IndexMap::new();
        for a in &args {
            if let Some(g) = &a.group_name {
                arg_groups
                    .entry(g.clone())
                    .or_default()
                    .push(a.cli_name.clone());
            }
        }
        Page {
            name: name.to_string(),
            event_class: event_class.to_string(),
            kind,
            args,
            commands: Vec::new(),
            has_commands: false,
            related_items: Vec::new(),
            doc: ReSTDocument::new(),
            arg_groups,
            documented_arg_groups: Vec::new(),
        }
    }

    fn is_operation(&self) -> bool {
        matches!(self.kind, Kind::Operation { .. })
    }

    fn is_basic(&self) -> bool {
        matches!(self.kind, Kind::Basic { .. })
    }

    fn is_waiter_basic(&self) -> bool {
        matches!(self.kind, Kind::Basic { waiter: true, .. })
    }

    /// `docevents.generate_events` with the registered handlers, returning
    /// the document (`doc.getvalue()`).
    pub(crate) fn generate(mut self) -> Result<String, String> {
        self.doc_meta_description();
        self.doc_title();
        self.doc_description();
        self.doc_synopsis_start();
        for i in 0..self.args.len() {
            if !self.args[i].undocumented {
                self.doc_synopsis_option(i);
            }
        }
        self.doc_synopsis_end();
        self.doc_options_start();
        for i in 0..self.args.len() {
            if self.args[i].undocumented {
                continue;
            }
            self.doc_option(i);
            self.doc_option_example(i);
        }
        self.doc_global_option();
        self.doc_subitems_start();
        let mut commands = self.commands.clone();
        commands.sort();
        for c in &commands {
            self.doc_subitem(c);
        }
        self.doc_examples();
        self.doc_output()?;
        if !self.related_items.is_empty() {
            self.doc.h2("See Also");
        }
        let mut related = self.related_items.clone();
        related.sort();
        for r in &related {
            self.doc.write("* ");
            self.doc.sphinx_reference_label(r);
            self.doc.write("\n");
        }
        Ok(self.doc.getvalue())
    }

    fn reference(&self) -> String {
        self.event_class.replace('.', " ")
    }

    fn doc_meta_description(&mut self) {
        let version = crate::model::REFERENCE_VERSION.trim();
        let reference = self.reference();
        match self.kind {
            Kind::Service { .. } => {
                self.doc.writeln(".. meta::");
                self.doc.writeln(&format!(
                    "   :description: Learn about the AWS CLI {version} {reference} commands."
                ));
                self.doc.writeln("");
            }
            Kind::Operation { .. } | Kind::Basic { .. } => {
                self.doc.writeln(".. meta::");
                self.doc.writeln(&format!(
                    "   :description: Use the AWS CLI {version} to run the {reference} command."
                ));
                self.doc.writeln("");
            }
            _ => {}
        }
    }

    fn doc_title(&mut self) {
        match self.kind {
            Kind::TopicLister | Kind::Topic => {
                self.doc.new_paragraph();
                self.doc
                    .link_target_definition(&format!("cli:aws help {}", self.name), "");
                let title = if matches!(self.kind, Kind::Topic) {
                    TopicTagDb::load().single_value(&self.name, "title")
                } else {
                    "AWS CLI Topic Guide".to_string()
                };
                self.doc.h1(&title);
            }
            _ => {
                self.doc.new_paragraph();
                let mut reference = self.reference();
                if reference != "aws" {
                    reference = format!("aws {reference}");
                }
                self.doc.writeln(&format!(".. _cli:{reference}:"));
                let name = self.name.clone();
                self.doc.h1(&name);
            }
        }
    }

    fn doc_description(&mut self) {
        if matches!(
            self.event_class.as_str(),
            "ec2.authorize-security-group-ingress"
                | "ec2.authorize-security-group-egress"
                | "ec2.revoke-security-group-ingress"
        ) {
            self.doc.new_paragraph();
            self.doc.start_note();
            self.doc
                .include_doc_string("To specify multiple rules in a single command use the <code>--ip-permissions</code> option");
            self.doc.end_note();
        }
        match &self.kind {
            Kind::Provider { description, .. } => {
                let description = description.clone();
                self.doc.h2("Description");
                self.doc.include_doc_string(&description);
                self.doc.new_paragraph();
            }
            Kind::Service { documentation } => {
                let documentation = documentation.clone();
                self.doc.h2("Description");
                self.doc.include_doc_string(&documentation);
            }
            Kind::Operation {
                op, documentation, ..
            } => {
                let (op, documentation) = (*op, documentation.clone());
                self.doc.h2("Description");
                self.doc.include_doc_string(&documentation);
                if op.model.metadata_str("uid").is_some() {
                    self.doc.new_paragraph();
                    self.doc.write("See also: ");
                    self.doc.external_link("AWS API Documentation");
                    self.doc.writeln("");
                }
                if self.uses_document_types() {
                    self.doc.new_paragraph();
                    self.doc.writeln(&format!(
                        "``{}`` uses document type values. Document types follow the JSON data model where valid values are: strings, numbers, booleans, null, arrays, and objects. For command input, options and nested parameters that are labeled with the type ``document`` must be provided as JSON. Shorthand syntax does not support document types.",
                        self.name
                    ));
                }
            }
            Kind::TopicLister => {
                self.doc.h2("Description");
                self.doc.include_doc_string(TOPIC_DESCRIPTION);
                self.doc.new_paragraph();
            }
            Kind::Topic => {
                let contents = data::topic_file(&format!("{}.rst", self.name)).unwrap_or_default();
                self.doc.writeln(&remove_tags_from_content(&contents));
                self.doc.new_paragraph();
            }
            Kind::Basic { description, .. } => {
                let description = description.clone();
                self.doc.h2("Description");
                self.doc.write(&description);
                self.doc.new_paragraph();
            }
        }
        if self.event_class == "iot-data" || self.event_class.starts_with("iot-data.") {
            self.doc.start_note();
            self.doc.writeln("For production code it is strongly recommended to use the custom endpoint for your account (retrievable via the iot describe-endpoint command) to ensure best availability and reachability of the service. The default endpoints (intended for testing purposes only) can be found at https://docs.aws.amazon.com/general/latest/gr/iot-core.html#iot-core-data-plane-endpoints");
            self.doc.end_note();
        }
        if let Kind::Operation { op, .. } = &self.kind {
            let op = *op;
            self.add_paging_description(&op);
        }
    }

    /// `paginate.add_paging_description`.
    fn add_paging_description(&mut self, op: &Operation<'_>) {
        let Some(config) = op.model.paginator(op.name) else {
            return;
        };
        self.doc.new_paragraph();
        self.doc.writeln(&format!(
            "``{}`` is a paginated operation. Multiple API calls may be issued in order to retrieve the entire data set of results. You can disable pagination by providing the ``--no-paginate`` argument.",
            self.name
        ));
        let result_key = config.get("result_key");
        let queries: Vec<String> = match (
            result_key.and_then(Node::as_array),
            result_key.and_then(Node::as_str),
        ) {
            (Some(a), _) if !a.is_empty() => a.iter().map(py_str).collect(),
            (_, Some(s)) if !s.is_empty() => vec![s.to_string()],
            _ => return,
        };
        let queries = queries
            .iter()
            .map(|q| format!("``{q}``"))
            .collect::<Vec<_>>()
            .join(", ");
        self.doc.writeln(&format!(
            "When using ``--output text`` and the ``--query`` argument on a paginated response, the ``--query`` argument must extract data from the results of the following query expressions: {queries}"
        ));
    }

    /// `utils.operation_uses_document_types`.
    fn uses_document_types(&self) -> bool {
        let Kind::Operation { op, output, .. } = &self.kind else {
            return false;
        };
        let mut visited = Vec::new();
        if let Some(input) = op.input() {
            walk_shapes(&input, &mut Vec::new(), &mut visited);
        }
        if let Some(output) = output {
            walk_shapes(output, &mut Vec::new(), &mut visited);
        }
        visited.iter().any(is_document_type)
    }

    fn doc_synopsis_start(&mut self) {
        match &self.kind {
            Kind::Provider {
                synopsis, usage, ..
            } => {
                let (synopsis, usage) = (synopsis.clone(), usage.clone());
                self.doc.h2("Synopsis");
                self.doc.codeblock(&synopsis);
                self.doc.include_doc_string(&usage);
            }
            Kind::Service { .. } | Kind::TopicLister | Kind::Topic => {}
            Kind::Basic { waiter: true, .. } => {}
            Kind::Basic { synopsis, .. } if !synopsis.is_empty() => {
                let synopsis = synopsis.clone();
                self.doc.h2("Synopsis");
                self.doc.start_codeblock();
                self.doc.writeln(&synopsis);
            }
            _ => {
                self.documented_arg_groups.clear();
                self.doc.h2("Synopsis");
                self.doc.start_codeblock();
                let name = self.name.clone();
                self.doc.writeln(&name);
            }
        }
    }

    fn doc_synopsis_option(&mut self, i: usize) {
        let arg = self.args[i].clone();
        match &self.kind {
            Kind::Operation { .. } | Kind::TopicLister | Kind::Topic => {
                let option_str = if let Some(g) = arg
                    .group_name
                    .as_ref()
                    .filter(|g| self.arg_groups.contains_key(*g))
                {
                    if self.documented_arg_groups.contains(g) {
                        return;
                    }
                    self.documented_arg_groups.push(g.clone());
                    self.arg_groups[g].join(" | ")
                } else if arg.cli_name.starts_with("--") {
                    format!("{} <value>", arg.cli_name)
                } else {
                    format!("<{}>", arg.cli_name)
                };
                let option_str = if arg.required || arg.document_as_required {
                    option_str
                } else {
                    format!("[{option_str}]")
                };
                self.doc.writeln(&option_str);
            }
            Kind::Basic {
                synopsis,
                waiter: false,
                ..
            } if synopsis.is_empty() => {
                let option_str = if let Some(s) = &arg.synopsis {
                    s.clone()
                } else if let Some(g) = arg
                    .group_name
                    .as_ref()
                    .filter(|g| self.arg_groups.contains_key(*g))
                {
                    if self.documented_arg_groups.contains(g) {
                        return;
                    }
                    self.documented_arg_groups.push(g.clone());
                    self.arg_groups[g].join(" | ")
                } else if arg.cli_type_name == "boolean" {
                    arg.cli_name.clone()
                } else if arg.nargs.as_deref() == Some("+") {
                    format!("{} <value> [<value>...]", arg.cli_name)
                } else {
                    format!("{} <value>", arg.cli_name)
                };
                let option_str = if arg.required || arg.positional_arg {
                    option_str
                } else {
                    format!("[{option_str}]")
                };
                self.doc.writeln(&option_str);
            }
            _ => {}
        }
    }

    fn doc_synopsis_end(&mut self) {
        let base = match &self.kind {
            Kind::Provider { .. } => {
                self.doc.new_paragraph();
                return;
            }
            Kind::Service { .. } | Kind::TopicLister | Kind::Topic => return,
            Kind::Basic { waiter: true, .. } => return,
            Kind::Basic { synopsis, .. } => synopsis.is_empty() && !self.has_commands,
            Kind::Operation { .. } => true,
        };
        if base {
            let file = data::example_file("global_synopsis.rst").unwrap_or_default();
            self.doc.write_from_file(&file);
            self.doc.end_codeblock();
            self.documented_arg_groups.clear();
        } else {
            self.doc.end_codeblock();
        }
    }

    fn doc_options_start(&mut self) {
        match self.kind {
            Kind::Provider { .. } | Kind::Service { .. } | Kind::TopicLister | Kind::Topic => {}
            Kind::Basic { waiter: true, .. } => {}
            _ => {
                self.doc.h2("Options");
                if self.args.is_empty() {
                    self.doc.write("*None*\n");
                }
            }
        }
    }

    fn doc_option(&mut self, i: usize) {
        if matches!(self.kind, Kind::Provider { .. } | Kind::Service { .. })
            || self.is_waiter_basic()
        {
            return;
        }
        let arg = self.args[i].clone();
        let required_field = if arg.required || arg.document_as_required {
            " [required]"
        } else {
            ""
        };
        let name = if let Some(g) = arg
            .group_name
            .as_ref()
            .filter(|g| self.arg_groups.contains_key(*g))
        {
            if self.documented_arg_groups.contains(g) {
                return;
            }
            let name = self.arg_groups[g]
                .iter()
                .map(|n| format!("``{n}``"))
                .collect::<Vec<_>>()
                .join(" | ");
            self.documented_arg_groups.push(g.clone());
            name
        } else {
            format!("``{}``", arg.cli_name)
        };
        self.doc.write(&format!(
            "{name} ({}){required_field}\n",
            argument_type_name(arg.model.as_ref(), &arg.cli_type_name)
        ));
        self.doc.indent();
        self.doc.include_doc_string(&arg.documentation);
        if let Some(model) = &arg.model {
            if is_streaming_blob(model) {
                self.add_streaming_blob_note();
            }
            if is_tagged_union(model) {
                self.add_tagged_union_note(model);
            }
            self.document_enums(model);
            self.document_constraints(model);
            self.document_nested_structure(model);
        }
        self.doc.dedent();
        self.doc.new_paragraph();
        if self.event_class == "route53.create-hosted-zone" && arg.name == "hosted-zone-config" {
            self.doc.include_doc_string(
                "<p>Note do <b>not</b> include <code>PrivateZone</code> in this input structure. Its value is returned in the output to the command.</p>",
            );
        }
    }

    fn add_streaming_blob_note(&mut self) {
        self.doc.start_note();
        self.doc.writeln("This argument is of type: streaming blob. Its value must be the path to a file (e.g. ``path/to/file``) and must **not** be prefixed with ``file://`` or ``fileb://``");
        self.doc.end_note();
    }

    fn add_tagged_union_note(&mut self, shape: &Shape<'_>) {
        self.doc.start_note();
        let members = shape
            .members()
            .iter()
            .map(|(k, _)| format!("``{k}``"))
            .collect::<Vec<_>>()
            .join(", ");
        self.doc.writeln(&format!(
            "This is a Tagged Union structure. Only one of the following top level keys can be set: {members}."
        ));
        self.doc.end_note();
    }

    fn document_enums(&mut self, shape: &Shape<'_>) {
        if is_string(shape) {
            let values = enum_values(shape);
            if !values.is_empty() {
                self.doc.new_paragraph();
                self.doc.write("Possible values:");
                self.doc.start_ul();
                for v in values {
                    self.doc.li(&format!("``{v}``"));
                }
                self.doc.end_ul();
            }
        }
    }

    fn document_constraints(&mut self, shape: &Shape<'_>) {
        let constraints = ["min", "max", "pattern"];
        if !constraints.iter().any(|c| shape.get(c).is_some()) {
            return;
        }
        self.doc.new_paragraph();
        self.doc.write("Constraints:");
        self.doc.start_ul();
        for c in constraints {
            if let Some(v) = shape.get(c).filter(|v| !v.is_null()) {
                self.doc.li(&format!("{c}: ``{}``", py_str(v)));
            }
        }
        self.doc.end_ul();
    }

    fn document_nested_structure(&mut self, shape: &Shape<'a>) {
        match shape.type_name() {
            "structure" => {
                let required = shape.required();
                for (name, member) in shape.members() {
                    let is_required = required.contains(&name);
                    self.doc_member(name, &member, &mut vec![shape.name], is_required);
                }
            }
            "list" => {
                if let Some(m) = shape.list_member() {
                    self.doc_member("", &m, &mut vec![shape.name], false);
                }
            }
            "map" => {
                if let Some(k) = shape.map_key() {
                    let key_name = k.get_str("locationName").unwrap_or("key").to_string();
                    self.doc_member(&key_name, &k, &mut vec![shape.name], false);
                }
                if let Some(v) = shape.map_value() {
                    let value_name = v.get_str("locationName").unwrap_or("value").to_string();
                    self.doc_member(&value_name, &v, &mut vec![shape.name], false);
                }
            }
            _ => {}
        }
    }

    fn doc_member(
        &mut self,
        member_name: &str,
        shape: &Shape<'a>,
        stack: &mut Vec<&'a str>,
        required: bool,
    ) {
        if stack.contains(&shape.name) && stack.iter().filter(|n| **n == shape.name).count() > 1 {
            if shape.type_name() == "structure" {
                self.doc.write("( ... recursive ... )");
            }
            return;
        }
        stack.push(shape.name);
        self.do_doc_member(member_name, shape, stack, required);
        stack.pop();
    }

    fn do_doc_member(
        &mut self,
        member_name: &str,
        shape: &Shape<'a>,
        stack: &mut Vec<&'a str>,
        required: bool,
    ) {
        let docs = shape_doc(shape);
        let type_name = argument_type_name(Some(shape), shape.type_name());
        let mut parameter_string = if member_name.is_empty() {
            format!("({type_name})")
        } else {
            format!("{member_name} -> ({type_name})")
        };
        if required {
            parameter_string.push_str(" [required]");
        }
        self.doc.write(&parameter_string);
        self.doc.indent();
        self.doc.new_paragraph();
        self.doc.include_doc_string(&docs);
        if is_tagged_union(shape) {
            self.add_tagged_union_note(shape);
        }
        self.document_enums(shape);
        self.document_constraints(shape);
        self.doc.new_paragraph();
        match shape.type_name() {
            "structure" => {
                let req = shape.required();
                for (name, member) in shape.members() {
                    self.doc_member(name, &member, stack, req.contains(&name));
                }
            }
            "map" => {
                if let Some(k) = shape.map_key() {
                    let key_name = k.get_str("locationName").unwrap_or("key").to_string();
                    self.doc_member(&key_name, &k, stack, false);
                }
                if let Some(v) = shape.map_value() {
                    let value_name = v.get_str("locationName").unwrap_or("value").to_string();
                    self.doc_member(&value_name, &v, stack, false);
                }
            }
            "list" => {
                if let Some(m) = shape.list_member() {
                    self.doc_member("", &m, stack, false);
                }
            }
            _ => {}
        }
        self.doc.dedent();
        self.doc.new_paragraph();
    }

    /// `OperationDocumentEventHandler._json_example_value_name`.
    fn json_example_value_name(shape: &Shape<'_>, include_enum_values: bool) -> String {
        if is_string(shape) {
            let values = enum_values(shape);
            if !values.is_empty() && include_enum_values {
                values
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join("|")
            } else {
                "\"string\"".to_string()
            }
        } else if shape.type_name() == "boolean" {
            "true|false".to_string()
        } else {
            shape.type_name().to_string()
        }
    }

    fn json_example(&mut self, shape: &Shape<'a>, stack: &mut Vec<&'a str>) {
        if stack.contains(&shape.name) && stack.iter().filter(|n| **n == shape.name).count() > 1 {
            if shape.type_name() == "structure" {
                self.doc.write("{ ... recursive ... }");
            }
            return;
        }
        stack.push(shape.name);
        self.do_json_example(shape, stack);
        stack.pop();
    }

    fn do_json_example(&mut self, shape: &Shape<'a>, stack: &mut Vec<&'a str>) {
        match shape.type_name() {
            "list" => {
                let member = shape.list_member().expect("list member");
                self.doc.write("[");
                if is_scalar(member.type_name()) {
                    self.doc.write(&format!(
                        "{}, ...",
                        Self::json_example_value_name(&member, true)
                    ));
                } else {
                    self.doc.indent();
                    self.doc.new_line();
                    self.json_example(&member, stack);
                    self.doc.new_line();
                    self.doc.write("...");
                    self.doc.dedent();
                    self.doc.new_line();
                }
                self.doc.write("]");
            }
            "map" => {
                let key = shape.map_key().expect("map key");
                let value = shape.map_value().expect("map value");
                self.doc.write("{");
                self.doc.indent();
                self.doc
                    .write(&format!("{}: ", Self::json_example_value_name(&key, true)));
                if is_scalar(value.type_name()) {
                    self.doc.write(&Self::json_example_value_name(&value, true));
                } else {
                    self.doc.indent();
                    self.json_example(&value, stack);
                    self.doc.dedent();
                }
                self.doc.new_line();
                self.doc.write("...");
                self.doc.dedent();
                self.doc.write("}");
            }
            "structure" => {
                if is_document_type(shape) {
                    self.doc.write("{...}");
                } else {
                    self.doc.write("{");
                    self.doc.indent();
                    self.doc.new_line();
                    let members = shape.members();
                    let n = members.len();
                    for (i, (name, member)) in members.into_iter().enumerate() {
                        let t = member.type_name();
                        if is_scalar(t) {
                            self.doc.write(&format!(
                                "\"{name}\": {}",
                                Self::json_example_value_name(&member, true)
                            ));
                        } else if matches!(t, "structure" | "map" | "list") {
                            self.doc.write(&format!("\"{name}\": "));
                            self.json_example(&member, stack);
                        }
                        if i + 1 < n {
                            self.doc.write(",");
                            self.doc.new_line();
                        }
                    }
                    self.doc.dedent();
                    self.doc.new_line();
                    self.doc.write("}");
                }
            }
            _ => {}
        }
    }

    fn doc_option_example(&mut self, i: usize) {
        if !(self.is_operation() || self.is_basic()) {
            return;
        }
        let arg = self.args[i].clone();
        if let Some(g) = &arg.group_name
            && self.arg_groups.contains_key(g)
            && self.documented_arg_groups.contains(g)
        {
            return;
        }
        let mut parts = self.event_class.split('.');
        let service = parts.next().unwrap_or_default().to_string();
        let operation = parts.next().unwrap_or_default().to_string();
        if let Some(model) = &arg.model
            && supports_shorthand(model)
        {
            match shorthand::generate(&arg, model, &service, &operation) {
                shorthand::Example::DontDoc => return,
                shorthand::Example::Text(t) if !t.is_empty() => {
                    self.doc.new_paragraph();
                    self.doc.write("Shorthand Syntax");
                    self.doc.start_codeblock();
                    for line in t.lines() {
                        self.doc.writeln(line);
                    }
                    self.doc.end_codeblock();
                }
                shorthand::Example::Text(_) => {}
            }
        }
        let list_member = arg
            .model
            .as_ref()
            .filter(|m| m.type_name() == "list")
            .and_then(Shape::list_member)
            .filter(|m| is_scalar(m.type_name()));
        if let Some(member) = list_member {
            self.doc.new_paragraph();
            self.doc.write("Syntax");
            self.doc.start_codeblock();
            let example_type = Self::json_example_value_name(&member, false);
            self.doc
                .write(&format!("{example_type} {example_type} ..."));
            self.doc.end_codeblock();
            self.doc.new_paragraph();
        } else if !is_scalar(&arg.cli_type_name) {
            self.doc.new_paragraph();
            self.doc.write("JSON Syntax");
            self.doc.start_codeblock();
            if let Some(model) = &arg.model {
                self.json_example(model, &mut Vec::new());
            }
            self.doc.end_codeblock();
            self.doc.new_paragraph();
        }
    }

    fn doc_global_option(&mut self) {
        let base = match self.kind {
            Kind::Provider { .. } | Kind::Operation { .. } => true,
            Kind::Basic { .. } => !self.has_commands,
            _ => false,
        };
        if base {
            self.doc.h2("Global Options");
            let file = data::example_file("global_options.rst").unwrap_or_default();
            self.doc.write_from_file(&file);
        }
    }

    fn doc_subitems_start(&mut self) {
        match self.kind {
            Kind::Provider { .. } => {
                self.doc.h2("Available Services");
                self.doc.toctree();
            }
            Kind::Service { .. } => {
                self.doc.h2("Available Commands");
                self.doc.toctree();
            }
            Kind::Basic { .. } if self.has_commands => {
                self.doc.h2("Available Commands");
                self.doc.toctree();
            }
            Kind::TopicLister => self.topic_listing(),
            _ => {}
        }
    }

    /// `TopicListerDocumentEventHandler.doc_subitems_start`.
    fn topic_listing(&mut self) {
        let db = TopicTagDb::load();
        self.doc.h2("Available Topics");
        let categories = db.query("category");
        let mut names: Vec<&String> = categories.keys().collect();
        names.sort();
        for category in names {
            self.doc.h3(category);
            self.doc.new_paragraph();
            let mut topics = categories[category].clone();
            topics.sort();
            for topic in topics {
                let description = db.single_value(&topic, "description");
                self.doc.write("* ");
                self.doc.sphinx_reference_label(&topic);
                self.doc.write(&format!(": {description}\n"));
            }
        }
    }

    fn doc_subitem(&mut self, name: &str) {
        match self.kind {
            Kind::Provider { .. } | Kind::Service { .. } => self.doc.tocitem(name),
            Kind::Basic { .. } if self.has_commands => self.doc.tocitem(name),
            _ => {}
        }
    }

    fn doc_examples(&mut self) {
        if self.event_class.contains('.') {
            let path = format!("{}.rst", self.event_class.replace('.', "/"));
            if let Some(contents) = data::example_file(&path) {
                self.doc.h2("Examples");
                self.doc.start_note();
                self.doc.include_doc_string(
                    "<p>To use the following examples, you must have the AWS CLI installed and configured. See the <a href='https://docs.aws.amazon.com/cli/latest/userguide/cli-chap-getting-started.html'>Getting started guide</a> in the <i>AWS CLI User Guide</i> for more information.</p><p>Unless otherwise stated, all examples have unix-like quotation rules. These examples will need to be adapted to your terminal's quoting rules. See <a href='https://docs.aws.amazon.com/cli/latest/userguide/cli-usage-parameters-quoting-strings.html'>Using quotation marks with strings</a> in the <i>AWS CLI User Guide</i>.</p>",
                );
                self.doc.end_note();
                for line in py_readlines(&contents) {
                    self.doc.write(line);
                }
            }
        }
        if let Kind::Basic { examples, .. } = &self.kind
            && !examples.is_empty()
        {
            let examples = examples.clone();
            self.doc.h2("Examples");
            self.doc.write(&examples);
        }
    }

    fn doc_output(&mut self) -> Result<(), String> {
        let Kind::Operation { output, .. } = &self.kind else {
            return Ok(());
        };
        let output = *output;
        self.doc.h2("Output");
        match output.filter(|o| !o.members().is_empty()) {
            None => self.doc.write("None"),
            Some(o) => {
                for (name, member) in o.members() {
                    self.doc_member(name, &member, &mut Vec::new(), false);
                }
            }
        }
        match self.event_class.as_str() {
            "datapipeline.get-pipeline-definition" => {
                self.pop_to_output()?;
                self.doc.write("======\nOutput\n======");
                self.doc.write("\nThe output of this command is the pipeline definition, which is documented in the `Pipeline Definition File Syntax <http://docs.aws.amazon.com/datapipeline/latest/DeveloperGuide/dp-writing-pipeline-definition.html>`__");
            }
            "s3api.select-object-content" => {
                self.pop_to_output()?;
                self.doc.write("======\nOutput\n======\n");
                self.doc.write("This command generates no output.  The selected object content is written to the specified outfile.\n");
            }
            _ => {}
        }
        if self.event_class.starts_with("s3api.") {
            self.document_expires_string();
        }
        Ok(())
    }

    fn pop_to_output(&mut self) -> Result<(), String> {
        loop {
            match self.doc.pop_write_opt() {
                Some(w) if w == "======\nOutput\n======" => return Ok(()),
                Some(_) => {}
                None => {
                    return Err(format!(
                        "Could not find the \"output\" section for the command: {}",
                        self.name
                    ));
                }
            }
        }
    }

    /// `s3events.document_expires_string`.
    fn document_expires_string(&mut self) {
        let Some(idx) = self.doc.find_last_write("Expires -> (timestamp)") else {
            return;
        };
        let pad = " ".repeat(self.doc.indentation() * 2);
        let pad1 = " ".repeat((self.doc.indentation() + 1) * 2);
        let writes = [
            format!("\n\n\n{pad}"),
            ".. note::".to_string(),
            format!("\n\n\n{pad1}"),
            "This member has been deprecated. Please use `ExpiresString` instead.\n".to_string(),
            format!("\n\n{pad}"),
            format!("\n\n{pad}"),
            "ExpiresString -> (string)\n\n".to_string(),
            "\tThe raw, unparsed value of the ``Expires`` field.".to_string(),
            format!("\n\n{pad}"),
        ];
        for (i, w) in writes.into_iter().enumerate() {
            self.doc.insert_write(idx + i + 4, w);
        }
    }
}
