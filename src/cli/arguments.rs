//! Argument tables of model-driven operations (`awscli/clidriver.py`
//! `ServiceOperation._create_argument_table`, `awscli/arguments.py`) and
//! the generic `building-argument-table` customizations
//! (`awscli/customizations/streamingoutputarg.py`, `argrename.py`,
//! `toplevelbool.py`, `cliinput.py`, `paginate.py`,
//! `generatecliskeleton.py`, `ec2/addcount.py`, `ec2/runinstances.py`).

use std::sync::OnceLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value as Json;

use super::argparse::{Action, Kind, Nargs, NsVal, Parser, TypeFn};
use super::tables::XFORM_CACHE;
use crate::model::{Operation, Shape};

/// `botocore.xform_name`.
pub fn xform_name(name: &str, sep: char) -> String {
    if name.contains(sep) {
        return name.to_string();
    }
    if let Some((_, _, v)) = XFORM_CACHE.iter().find(|(n, s, _)| *n == name && *s == sep) {
        return v.to_string();
    }
    static SPECIAL: OnceLock<Regex> = OnceLock::new();
    static FIRST: OnceLock<Regex> = OnceLock::new();
    static END: OnceLock<Regex> = OnceLock::new();
    let special = SPECIAL.get_or_init(|| Regex::new("[A-Z]{2,}s$").expect("regex"));
    let first = FIRST.get_or_init(|| Regex::new("(.)([A-Z][a-z]+)").expect("regex"));
    let end = END.get_or_init(|| Regex::new("([a-z0-9])([A-Z])").expect("regex"));
    let mut name = name.to_string();
    if let Some(m) = special.find(&name) {
        let matched = m.as_str().to_string();
        name = format!(
            "{}{sep}{}",
            &name[..name.len() - matched.len()],
            matched.to_lowercase()
        );
    }
    let s1 = first.replace_all(&name, format!("${{1}}{sep}${{2}}").as_str());
    end.replace_all(&s1, format!("${{1}}{sep}${{2}}").as_str())
        .to_lowercase()
}

/// The argument classes of the reference, with what each one needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ArgKind {
    /// `CLIArgument`.
    Cli,
    /// `ListArgument`.
    List,
    /// `BooleanArgument` (`store_true`, or `store_false` for the `no-`
    /// form), writing to `dest`.
    Boolean { negative: bool, dest: String },
    /// `toplevelbool.PositiveBooleanArgument`.
    PositiveBool,
    /// `toplevelbool.NegativeBooleanParameter`.
    NegativeBool,
    /// `paginate.PageArgument`.
    Page {
        parse_type: String,
        serialized: &'static str,
        min: Option<Json>,
        max: Option<Json>,
    },
    /// `cliinput.CliInputJSONArgument` / `CliInputYAMLArgument`.
    CliInput { yaml: bool },
    /// `generatecliskeleton.GenerateCliSkeletonArgument`.
    Skeleton,
    /// `streamingoutputarg.StreamingOutputArgument`.
    Outfile,
    /// `ec2.addcount.CountArgument`.
    Count,
    /// `ec2.runinstances.SecondaryPrivateIpAddressesArgument`.
    SecondaryIps,
    /// `ec2.runinstances.SecondaryPrivateIpAddressCountArgument`.
    SecondaryIpCount,
    /// `ec2.runinstances.AssociatePublicIpAddressArgument` /
    /// `NoAssociatePublicIpAddressArgument`.
    AssociatePublicIp { negative: bool },
}

/// One entry of an argument table.
#[derive(Debug, Clone)]
pub(crate) struct Arg<'m> {
    pub name: String,
    pub kind: ArgKind,
    /// `argument_model` (the member shape for model arguments).
    pub shape: Option<Shape<'m>>,
    pub serialized_name: String,
    pub required: bool,
    /// `_UNDOCUMENTED`.
    pub undocumented: bool,
}

impl<'m> Arg<'m> {
    fn model(
        name: &str,
        kind: ArgKind,
        shape: Shape<'m>,
        serialized: &str,
        required: bool,
    ) -> Arg<'m> {
        Arg {
            name: name.to_string(),
            kind,
            shape: Some(shape),
            serialized_name: serialized.to_string(),
            required,
            undocumented: false,
        }
    }

    fn custom(name: &str, kind: ArgKind) -> Arg<'m> {
        Arg {
            name: name.to_string(),
            kind,
            shape: None,
            serialized_name: String::new(),
            required: false,
            undocumented: false,
        }
    }

    pub(crate) fn py_name(&self) -> String {
        self.name.replace('-', "_")
    }

    pub(crate) fn cli_name(&self) -> String {
        match self.kind {
            ArgKind::Outfile => self.name.clone(),
            _ => format!("--{}", self.name),
        }
    }

    /// The Python class name (for the argument table fingerprints).
    #[cfg(test)]
    pub(crate) fn class_name(&self) -> &'static str {
        match self.kind {
            ArgKind::Cli => "CLIArgument",
            ArgKind::List => "ListArgument",
            ArgKind::Boolean { .. } => "BooleanArgument",
            ArgKind::PositiveBool => "PositiveBooleanArgument",
            ArgKind::NegativeBool => "NegativeBooleanParameter",
            ArgKind::Page { .. } => "PageArgument",
            ArgKind::CliInput { yaml: false } => "CliInputJSONArgument",
            ArgKind::CliInput { yaml: true } => "CliInputYAMLArgument",
            ArgKind::Skeleton => "GenerateCliSkeletonArgument",
            ArgKind::Outfile => "StreamingOutputArgument",
            ArgKind::Count => "CountArgument",
            ArgKind::SecondaryIps => "SecondaryPrivateIpAddressesArgument",
            ArgKind::SecondaryIpCount => "SecondaryPrivateIpAddressCountArgument",
            ArgKind::AssociatePublicIp { negative: false } => "AssociatePublicIpAddressArgument",
            ArgKind::AssociatePublicIp { negative: true } => "NoAssociatePublicIpAddressArgument",
        }
    }

    /// `add_to_parser`.
    pub(crate) fn add_to_parser(&self, parser: &mut Parser) {
        let cli_name = self.cli_name();
        let mut a = Action::option(&cli_name);
        match &self.kind {
            ArgKind::Cli => {
                a.ty = cli_type(self.shape.as_ref());
                a.required = self.required;
            }
            ArgKind::List => {
                a.ty = cli_type(self.shape.as_ref());
                a.nargs = Nargs::ZeroOrMore;
                a.required = self.required;
            }
            ArgKind::Boolean { negative, dest } => {
                a = Action::flag(
                    &cli_name,
                    if *negative {
                        Kind::StoreFalse
                    } else {
                        Kind::StoreTrue
                    },
                );
                a.default = NsVal::None;
                a.dest = dest.clone();
            }
            ArgKind::PositiveBool => {
                a.nargs = Nargs::Optional;
                a.default = NsVal::NotSpecified;
            }
            ArgKind::NegativeBool => {
                a = Action::flag(&cli_name, Kind::StoreTrue);
                a.default = NsVal::NotSpecified;
            }
            ArgKind::Page { parse_type, .. } => {
                a.ty = match parse_type.as_str() {
                    "string" => TypeFn::Str,
                    _ => TypeFn::Int,
                };
            }
            ArgKind::CliInput { .. } => {}
            ArgKind::Skeleton => {
                a.nargs = Nargs::Optional;
                a.const_ = Some("input".to_string());
                a.choices = Some(vec![
                    "input".to_string(),
                    "output".to_string(),
                    "yaml-input".to_string(),
                ]);
            }
            ArgKind::Outfile => {
                a = Action::positional(&self.name, Nargs::One);
                a.metavar = Some(self.py_name());
            }
            ArgKind::Count => {
                a.metavar = Some(self.py_name());
            }
            ArgKind::SecondaryIps => {
                a.nargs = Nargs::ZeroOrMore;
            }
            ArgKind::SecondaryIpCount => {
                a.ty = TypeFn::Int;
            }
            ArgKind::AssociatePublicIp { negative } => {
                a = Action::flag(
                    &cli_name,
                    if *negative {
                        Kind::StoreFalse
                    } else {
                        Kind::StoreTrue
                    },
                );
            }
        }
        if !matches!(self.kind, ArgKind::Outfile | ArgKind::Boolean { .. }) {
            a.dest = self.py_name();
        }
        parser.add(a);
    }
}

/// `CLIArgument.cli_type` (`TYPE_MAP`).
fn cli_type(shape: Option<&Shape<'_>>) -> TypeFn {
    match shape.map(Shape::type_name) {
        Some("float" | "double") => TypeFn::Float,
        Some("long") => TypeFn::Int,
        _ => TypeFn::Str,
    }
}

pub(crate) type ArgTable<'m> = IndexMap<String, Arg<'m>>;

/// `ARGUMENT_RENAMES` (`awscli/customizations/argrename.py`).
static ARGUMENT_RENAMES: &[(&str, &str)] = &[
    ("ec2.create-image.no-no-reboot", "reboot"),
    ("ec2.*.no-egress", "ingress"),
    ("ec2.*.no-disable-api-termination", "enable-api-termination"),
    ("swf.register-activity-type.version", "activity-version"),
    ("swf.register-workflow-type.version", "workflow-version"),
    ("datapipeline.*.query", "objects-query"),
    (
        "datapipeline.get-pipeline-definition.version",
        "pipeline-version",
    ),
    ("emr.*.job-flow-ids", "cluster-ids"),
    ("emr.*.job-flow-id", "cluster-id"),
    ("cloudsearchdomain.search.query", "search-query"),
    ("cloudsearchdomain.suggest.query", "suggest-query"),
    ("sns.subscribe.endpoint", "notification-endpoint"),
    ("deploy.*.s-3-location", "s3-location"),
    ("deploy.*.ec-2-tag-filters", "ec2-tag-filters"),
    ("codepipeline.get-pipeline.version", "pipeline-version"),
    (
        "codepipeline.create-custom-action-type.version",
        "action-version",
    ),
    (
        "codepipeline.delete-custom-action-type.version",
        "action-version",
    ),
    (
        "kinesisanalytics.add-application-output.output",
        "application-output",
    ),
    (
        "kinesisanalyticsv2.add-application-output.output",
        "application-output",
    ),
    (
        "route53.delete-traffic-policy.version",
        "traffic-policy-version",
    ),
    (
        "route53.get-traffic-policy.version",
        "traffic-policy-version",
    ),
    (
        "route53.update-traffic-policy-comment.version",
        "traffic-policy-version",
    ),
    ("gamelift.create-build.version", "build-version"),
    ("gamelift.update-build.version", "build-version"),
    ("gamelift.create-script.version", "script-version"),
    ("gamelift.update-script.version", "script-version"),
    ("route53domains.view-billing.start", "start-time"),
    ("route53domains.view-billing.end", "end-time"),
    ("apigateway.create-rest-api.version", "api-version"),
    ("apigatewayv2.create-api.version", "api-version"),
    ("apigatewayv2.update-api.version", "api-version"),
    ("pinpoint.get-campaign-version.version", "campaign-version"),
    ("pinpoint.get-segment-version.version", "segment-version"),
    ("pinpoint.delete-email-template.version", "template-version"),
    (
        "pinpoint.delete-in-app-template.version",
        "template-version",
    ),
    ("pinpoint.delete-push-template.version", "template-version"),
    ("pinpoint.delete-sms-template.version", "template-version"),
    ("pinpoint.delete-voice-template.version", "template-version"),
    ("pinpoint.get-email-template.version", "template-version"),
    ("pinpoint.get-in-app-template.version", "template-version"),
    ("pinpoint.get-push-template.version", "template-version"),
    ("pinpoint.get-sms-template.version", "template-version"),
    ("pinpoint.get-voice-template.version", "template-version"),
    ("pinpoint.update-email-template.version", "template-version"),
    (
        "pinpoint.update-in-app-template.version",
        "template-version",
    ),
    ("pinpoint.update-push-template.version", "template-version"),
    ("pinpoint.update-sms-template.version", "template-version"),
    ("pinpoint.update-voice-template.version", "template-version"),
    ("stepfunctions.send-task-success.output", "task-output"),
    ("clouddirectory.publish-schema.version", "schema-version"),
    ("mturk.list-qualification-types.query", "types-query"),
    (
        "workdocs.create-notification-subscription.endpoint",
        "notification-endpoint",
    ),
    ("workdocs.describe-users.query", "user-query"),
    ("lex-models.delete-bot.version", "bot-version"),
    ("lex-models.delete-intent.version", "intent-version"),
    ("lex-models.delete-slot-type.version", "slot-type-version"),
    ("lex-models.get-intent.version", "intent-version"),
    ("lex-models.get-slot-type.version", "slot-type-version"),
    ("lex-models.delete-bot-version.version", "bot-version"),
    ("lex-models.delete-intent-version.version", "intent-version"),
    (
        "lex-models.delete-slot-type-version.version",
        "slot-type-version",
    ),
    ("lex-models.get-export.version", "resource-version"),
    ("license-manager.get-grant.version", "grant-version"),
    ("license-manager.delete-grant.version", "grant-version"),
    ("license-manager.get-license.version", "license-version"),
    (
        "rekognition.create-stream-processor.output",
        "stream-processor-output",
    ),
    ("eks.create-cluster.version", "kubernetes-version"),
    ("eks.update-cluster-version.version", "kubernetes-version"),
    ("eks.create-nodegroup.version", "kubernetes-version"),
    ("eks.update-nodegroup-version.version", "kubernetes-version"),
    (
        "eks.update-cluster-components-version.version",
        "kubernetes-version",
    ),
    ("schemas.*.version", "schema-version"),
    ("sagemaker.delete-image-version.version", "version-number"),
    ("sagemaker.describe-image-version.version", "version-number"),
    ("sagemaker.list-aliases.version", "version-number"),
    ("sagemaker.update-image-version.version", "version-number"),
    ("iotwireless.*.lo-ra-wan", "lorawan"),
    ("codepipeline.get-action-type.version", "action-version"),
    ("ecs.*.no-enable-execute-command", "disable-execute-command"),
    ("ecs.execute-command.no-interactive", "non-interactive"),
    (
        "controltower.create-landing-zone.version",
        "landing-zone-version",
    ),
    (
        "controltower.update-landing-zone.version",
        "landing-zone-version",
    ),
    (
        "glue.get-unfiltered-partition-metadata.region",
        "resource-region",
    ),
    (
        "glue.get-unfiltered-partitions-metadata.region",
        "resource-region",
    ),
    (
        "glue.get-unfiltered-table-metadata.region",
        "resource-region",
    ),
    ("agent-toolkit.*.name", "skill-name"),
    ("agent-toolkit.search-skills.query", "search-query"),
];

/// `HIDDEN_ALIASES` (`awscli/customizations/argrename.py`).
static HIDDEN_ALIASES: &[(&str, &str)] = &[
    (
        "mgn.*.replication-servers-security-groups-ids",
        "replication-servers-security-groups-i-ds",
    ),
    ("mgn.*.source-server-ids", "source-server-i-ds"),
    (
        "mgn.*.replication-configuration-template-ids",
        "replication-configuration-template-i-ds",
    ),
    (
        "elasticache.create-replication-group.preferred-cache-cluster-azs",
        "preferred-cache-cluster-a-zs",
    ),
];

/// `utils.rename_argument`.
fn rename_argument(table: &mut ArgTable<'_>, existing: &str, new_name: &str) {
    let mut current = table.get(existing).expect("argument").clone();
    current.name = new_name.to_string();
    table.insert(new_name.to_string(), current);
    table.shift_remove(existing);
}

/// `utils.make_hidden_alias`.
fn make_hidden_alias(table: &mut ArgTable<'_>, existing: &str, alias: &str) {
    let current = table.get_mut(existing).expect("argument");
    let mut copy = current.clone();
    copy.name = alias.to_string();
    copy.undocumented = true;
    if current.required {
        copy.required = false;
        current.required = false;
    }
    table.insert(alias.to_string(), copy);
}

/// The argrename handlers registered on `building-argument-table.<event>`
/// (renames first, then hidden aliases, each in dict order).
fn apply_renames(table: &mut ArgTable<'_>, event: &str) {
    for (key, new_name) in ARGUMENT_RENAMES {
        let (portion, original) = key.rsplit_once('.').expect("rename key");
        if portion == event && table.contains_key(original) {
            rename_argument(table, original, new_name);
        }
    }
    for (key, alias) in HIDDEN_ALIASES {
        let (portion, original) = key.rsplit_once('.').expect("alias key");
        if portion == event && table.contains_key(original) {
            make_hidden_alias(table, original, alias);
        }
    }
}

/// `argprocess.detect_shape_structure`.
pub(crate) fn detect_shape_structure(shape: &Shape<'_>) -> String {
    fn inner(s: &Shape<'_>, stack: &mut Vec<String>) -> String {
        if stack.iter().any(|n| n == s.name) {
            return "recursive".to_string();
        }
        stack.push(s.name.to_string());
        let r = match s.type_name() {
            t if is_scalar_type(t) => "scalar".to_string(),
            "structure" => {
                let subs: Vec<String> = s.members().iter().map(|(_, m)| inner(m, stack)).collect();
                if subs.len() == 1 && subs.iter().all(|p| p == "scalar") {
                    "structure(scalar)".to_string()
                } else if subs.len() > 1 && subs.iter().all(|p| p == "scalar") {
                    "structure(scalars)".to_string()
                } else {
                    let mut set: Vec<String> = subs.clone();
                    set.sort();
                    set.dedup();
                    format!("structure({})", set.join(", "))
                }
            }
            "list" => match s.list_member() {
                Some(m) => format!("list-{}", inner(&m, stack)),
                None => "list-".to_string(),
            },
            "map" => match s.map_value() {
                Some(v) if is_scalar_type(v.type_name()) => "map-scalar".to_string(),
                Some(v) => format!("map-{}", inner(&v, stack)),
                None => "map-".to_string(),
            },
            _ => "None".to_string(),
        };
        stack.pop();
        r
    }
    inner(shape, &mut Vec::new())
}

/// `awscli.SCALAR_TYPES`.
pub(crate) fn is_scalar_type(t: &str) -> bool {
    matches!(
        t,
        "string" | "float" | "integer" | "long" | "boolean" | "double" | "blob" | "timestamp"
    )
}

/// `toplevelbool._qualifies_for_simplification`.
fn qualifies_for_simplification(shape: &Shape<'_>) -> bool {
    if detect_shape_structure(shape) == "structure(scalar)" {
        let members = shape.members();
        return members.len() == 1
            && members[0].0 == "Value"
            && members[0].1.type_name() == "boolean";
    }
    false
}

/// `toplevelbool.pull_up_bool`; returns the (positive, negative) pairs.
fn pull_up_bool(table: &mut ArgTable<'_>) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let values: Vec<Arg<'_>> = table.values().cloned().collect();
    for value in values {
        let Some(shape) = value.shape else { continue };
        if !matches!(
            value.kind,
            ArgKind::Cli
                | ArgKind::List
                | ArgKind::Boolean { .. }
                | ArgKind::PositiveBool
                | ArgKind::NegativeBool
        ) {
            continue;
        }
        if qualifies_for_simplification(&shape) {
            let positive = Arg {
                name: value.name.clone(),
                kind: ArgKind::PositiveBool,
                shape: Some(shape),
                serialized_name: value.serialized_name.clone(),
                required: false,
                undocumented: false,
            };
            let negative_name = format!("no-{}", value.name);
            let negative = Arg {
                name: negative_name.clone(),
                kind: ArgKind::NegativeBool,
                shape: Some(shape),
                serialized_name: value.serialized_name.clone(),
                required: false,
                undocumented: false,
            };
            pairs.push((positive.cli_name(), negative.cli_name()));
            table.insert(value.name.clone(), positive);
            table.insert(negative_name, negative);
        }
    }
    pairs
}

/// `paginate._get_input_tokens` plus the `limit_key`, in the API
/// namespace.
pub(crate) fn paginator_input_tokens(config: &serde_json::Map<String, Json>) -> Vec<String> {
    let mut out: Vec<String> = match config.get("input_token") {
        Some(Json::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(Json::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    };
    if let Some(Json::String(k)) = config.get("limit_key") {
        out.push(k.clone());
    }
    out
}

/// What the generic customizations recorded while building a table.
#[derive(Debug, Default, Clone)]
pub(crate) struct TableExtras<'m> {
    /// `toplevelbool` (positive, negative) CLI names.
    pub bool_pairs: Vec<(String, String)>,
    /// `paginate` shadowed arguments.
    pub shadowed: IndexMap<String, Arg<'m>>,
    /// The paginator config, when the operation can paginate.
    pub paginator: Option<&'m serde_json::Map<String, Json>>,
}

/// Where an operation command sits: its parent command name (the CLI
/// service name, or `wait` for waiter commands) and its own name.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CommandPath<'a> {
    pub parent: &'a str,
    pub name: &'a str,
}

/// `ServiceOperation._create_argument_table` and the
/// `building-argument-table.<parent>.<name>` handlers, in the order the
/// reference's emitter calls them: `building-argument-table.*`
/// (streamingoutputarg), `<parent>.*` (argrename, toplevelbool),
/// `<parent>.<name>` (argrename, ec2 addcount and runinstances), then
/// `building-argument-table` (cliinput, paginate, generatecliskeleton).
pub(crate) fn build_argument_table<'m>(
    operation: &Operation<'m>,
    path: CommandPath<'_>,
) -> (ArgTable<'m>, TableExtras<'m>) {
    let mut table: ArgTable<'m> = IndexMap::new();
    let mut extras = TableExtras::default();
    if let Some(input) = operation.input() {
        let required = input.required();
        for (member, shape) in input.members() {
            let cli = xform_name(member, '-');
            let is_token = shape.get_bool("idempotencyToken");
            let is_required = required.contains(&member) && !is_token;
            match shape.type_name() {
                "list" => {
                    table.insert(
                        cli.clone(),
                        Arg::model(&cli, ArgKind::List, shape, member, is_required),
                    );
                }
                "boolean" => {
                    let dest = cli.replace('-', "_");
                    table.insert(
                        cli.clone(),
                        Arg::model(
                            &cli,
                            ArgKind::Boolean {
                                negative: false,
                                dest: dest.clone(),
                            },
                            shape,
                            member,
                            is_required,
                        ),
                    );
                    let negative = format!("no-{cli}");
                    table.insert(
                        negative.clone(),
                        Arg::model(
                            &negative,
                            ArgKind::Boolean {
                                negative: true,
                                dest,
                            },
                            shape,
                            member,
                            false,
                        ),
                    );
                }
                _ => {
                    table.insert(
                        cli.clone(),
                        Arg::model(&cli, ArgKind::Cli, shape, member, is_required),
                    );
                }
            }
        }
    }

    if let Some(output) = operation.output()
        && has_streaming_output(&output)
    {
        let mut arg = Arg::custom("outfile", ArgKind::Outfile);
        arg.required = true;
        table.insert("outfile".to_string(), arg);
    }

    apply_renames(&mut table, &format!("{}.*", path.parent));
    if path.parent == "ec2" {
        extras.bool_pairs = pull_up_bool(&mut table);
    }

    apply_renames(&mut table, &format!("{}.{}", path.parent, path.name));
    if path.parent == "ec2" && path.name == "run-instances" {
        table.insert("count".to_string(), Arg::custom("count", ArgKind::Count));
        table.shift_remove("min-count");
        table.shift_remove("max-count");
        for (name, kind) in [
            ("secondary-private-ip-addresses", ArgKind::SecondaryIps),
            (
                "secondary-private-ip-address-count",
                ArgKind::SecondaryIpCount,
            ),
            (
                "associate-public-ip-address",
                ArgKind::AssociatePublicIp { negative: false },
            ),
            (
                "no-associate-public-ip-address",
                ArgKind::AssociatePublicIp { negative: true },
            ),
        ] {
            table.insert(name.to_string(), Arg::custom(name, kind));
        }
    }

    if !table.contains_key("outfile") {
        table.insert(
            "cli-input-json".to_string(),
            Arg::custom("cli-input-json", ArgKind::CliInput { yaml: false }),
        );
        table.insert(
            "cli-input-yaml".to_string(),
            Arg::custom("cli-input-yaml", ArgKind::CliInput { yaml: true }),
        );
    }
    if let Some(config) = operation.model.paginator(operation.name) {
        extras.paginator = Some(config);
        unify_paging_params(&mut table, &mut extras.shadowed, operation, config);
    }
    if !table.contains_key("outfile") {
        table.insert(
            "generate-cli-skeleton".to_string(),
            Arg::custom("generate-cli-skeleton", ArgKind::Skeleton),
        );
    }
    (table, extras)
}

/// `OperationModel.has_streaming_output`.
fn has_streaming_output(output: &Shape<'_>) -> bool {
    output
        .def
        .get("payload")
        .and_then(Json::as_str)
        .and_then(|p| output.member_shape(p))
        .is_some_and(|m| m.type_name() == "blob")
}

/// `paginate.unify_paging_params`.
fn unify_paging_params<'m>(
    table: &mut ArgTable<'m>,
    shadowed: &mut IndexMap<String, Arg<'m>>,
    operation: &Operation<'m>,
    config: &serde_json::Map<String, Json>,
) {
    for token in paginator_input_tokens(config) {
        let cli = xform_name(&token, '-');
        if let Some(a) = table.get_mut(&cli) {
            a.undocumented = true;
        }
    }
    let mut add = |table: &mut ArgTable<'m>, name: &str, arg: Arg<'m>| {
        if let Some(existing) = table.get(name) {
            shadowed.insert(name.to_string(), existing.clone());
        }
        table.insert(name.to_string(), arg);
    };
    let page = |name: &str, parse_type: &str, serialized: &'static str, min, max| Arg {
        name: name.to_string(),
        kind: ArgKind::Page {
            parse_type: parse_type.to_string(),
            serialized,
            min,
            max,
        },
        shape: None,
        serialized_name: serialized.to_string(),
        required: false,
        undocumented: false,
    };
    add(
        table,
        "starting-token",
        page("starting-token", "string", "StartingToken", None, None),
    );
    let mut type_name = "integer".to_string();
    if let Some(limit_key) = config.get("limit_key").and_then(Json::as_str) {
        let limit_shape = operation
            .input()
            .and_then(|i| i.member_shape(limit_key))
            .expect("limit_key member");
        type_name = limit_shape.type_name().to_string();
        let min = limit_shape.get("min").cloned();
        let max = limit_shape.get("max").cloned();
        add(
            table,
            "page-size",
            page("page-size", &type_name, "PageSize", min, max),
        );
    }
    add(
        table,
        "max-items",
        page("max-items", &type_name, "MaxItems", None, None),
    );
}

/// Build the `ArgTableArgParser` for an argument table, with the `help`
/// positional `ServiceOperation._add_help` appends.
pub(crate) fn operation_parser(table: &ArgTable<'_>, with_help: bool) -> Parser {
    let mut p = Parser::new();
    for arg in table.values() {
        arg.add_to_parser(&mut p);
    }
    if with_help {
        p.add(Action::positional("help", Nargs::Optional));
    }
    p
}

/// One line per argument, the same text `gen_fingerprints.py` hashes for
/// the reference (`key|class|cli_name|required|py_name|dest|undocumented`).
#[cfg(test)]
pub(crate) fn fingerprint(table: &ArgTable<'_>) -> String {
    let mut out = String::new();
    for (key, arg) in table {
        let dest = match &arg.kind {
            ArgKind::Boolean { dest, .. } => dest.clone(),
            _ => String::new(),
        };
        out.push_str(&format!(
            "{key}|{}|{}|{}|{}|{dest}|{}\n",
            arg.class_name(),
            arg.cli_name(),
            u8::from(arg.required),
            arg.py_name(),
            u8::from(arg.undocumented),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xform() {
        assert_eq!(xform_name("DescribeInstances", '-'), "describe-instances");
        assert_eq!(
            xform_name("ListHITsForQualificationType", '-'),
            "list-hits-for-qualification-type"
        );
        assert_eq!(xform_name("CreateDBSnapshot", '-'), "create-db-snapshot");
        assert_eq!(
            xform_name("DescribeDBInstances", '-'),
            "describe-db-instances"
        );
        assert_eq!(xform_name("GetOTelEnrichment", '-'), "get-o-tel-enrichment");
        assert_eq!(xform_name("S3Location", '-'), "s3-location");
        assert_eq!(xform_name("SSEKMSKeyId", '_'), "ssekms_key_id");
        assert_eq!(xform_name("ListARNs", '-'), "list-arns");
        assert_eq!(xform_name("LoRaWAN", '-'), "lo-ra-wan");
        assert_eq!(xform_name("VPCIds", '_'), "vpc_ids");
    }
}
