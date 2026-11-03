//! The parts of Python's `argparse` (3.14) the AWS CLI relies on:
//! `ArgumentParser._parse_known_args` with `allow_abbrev`, the
//! `conflict_handler='resolve'` option table, the nargs patterns, value
//! conversion, and the `argparse.ArgumentError` messages, with the
//! `_check_value` override of `awscli/argparser.py` `CLIArgParser`.

use std::sync::OnceLock;

use indexmap::IndexMap;
use num_bigint::BigInt;
use regex::Regex;

use super::difflib::get_close_matches;
use super::pynum;
use crate::pyfmt::str_repr;

/// `nargs` of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nargs {
    /// `None`: exactly one argument.
    One,
    /// `'?'`.
    Optional,
    /// `'*'`.
    ZeroOrMore,
    /// An integer (`0` for flags).
    Exact(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Store,
    StoreTrue,
    StoreFalse,
    Version,
}

/// The `type` callable of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeFn {
    Str,
    Int,
    Float,
}

/// A value in the parsed namespace.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NsVal {
    None,
    /// `toplevelbool._NOT_SPECIFIED`.
    NotSpecified,
    Bool(bool),
    Str(String),
    Int(BigInt),
    Float(f64),
    List(Vec<NsVal>),
}

impl NsVal {
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, NsVal::None)
    }

    /// Python truthiness.
    pub(crate) fn is_truthy(&self) -> bool {
        match self {
            NsVal::None => false,
            NsVal::NotSpecified => true,
            NsVal::Bool(b) => *b,
            NsVal::Str(s) => !s.is_empty(),
            NsVal::Int(i) => i != &BigInt::from(0),
            NsVal::Float(f) => *f != 0.0,
            NsVal::List(l) => !l.is_empty(),
        }
    }

    /// `str(value)` for scalars.
    pub(crate) fn py_str(&self) -> String {
        match self {
            NsVal::None => "None".to_string(),
            NsVal::NotSpecified => "<object>".to_string(),
            NsVal::Bool(true) => "True".to_string(),
            NsVal::Bool(false) => "False".to_string(),
            NsVal::Str(s) => s.clone(),
            NsVal::Int(i) => i.to_string(),
            NsVal::Float(f) => crate::pyfmt::float_repr(*f),
            NsVal::List(l) => format!(
                "[{}]",
                l.iter()
                    .map(|v| match v {
                        NsVal::Str(s) => str_repr(s),
                        other => other.py_str(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// One `add_argument` call.
#[derive(Debug, Clone)]
pub(crate) struct Action {
    pub option_strings: Vec<String>,
    pub dest: String,
    pub kind: Kind,
    pub nargs: Nargs,
    pub default: NsVal,
    pub const_: Option<String>,
    pub choices: Option<Vec<String>>,
    pub required: bool,
    pub ty: TypeFn,
    pub metavar: Option<String>,
}

impl Action {
    /// An optional (`--name`) store action with the argparse defaults.
    pub(crate) fn option(option_string: &str) -> Action {
        Action {
            option_strings: vec![option_string.to_string()],
            dest: option_string.trim_start_matches('-').replace('-', "_"),
            kind: Kind::Store,
            nargs: Nargs::One,
            default: NsVal::None,
            const_: None,
            choices: None,
            required: false,
            ty: TypeFn::Str,
            metavar: None,
        }
    }

    /// A positional action (`required` follows `_get_positional_kwargs`).
    pub(crate) fn positional(dest: &str, nargs: Nargs) -> Action {
        Action {
            option_strings: Vec::new(),
            dest: dest.to_string(),
            kind: Kind::Store,
            nargs,
            default: NsVal::None,
            const_: None,
            choices: None,
            required: !matches!(nargs, Nargs::Optional | Nargs::ZeroOrMore),
            ty: TypeFn::Str,
            metavar: None,
        }
    }

    pub(crate) fn flag(option_string: &str, kind: Kind) -> Action {
        let mut a = Action::option(option_string);
        a.kind = kind;
        a.nargs = Nargs::Exact(0);
        a.default = match kind {
            Kind::StoreTrue => NsVal::Bool(false),
            Kind::StoreFalse => NsVal::Bool(true),
            _ => NsVal::None,
        };
        a
    }

    /// `argparse._get_action_name`.
    fn name(&self) -> Option<String> {
        if !self.option_strings.is_empty() {
            Some(self.option_strings.join("/"))
        } else if let Some(m) = &self.metavar {
            Some(m.clone())
        } else {
            Some(self.dest.clone())
        }
    }
}

/// `argparse.ArgumentError` (and the `ArgumentParser.error` it turns into).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ArgumentError {
    pub argument_name: Option<String>,
    pub message: String,
}

impl ArgumentError {
    fn new(action: Option<&Action>, message: impl Into<String>) -> ArgumentError {
        ArgumentError {
            argument_name: action.and_then(Action::name),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ArgumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.argument_name {
            None => f.write_str(&self.message),
            Some(n) => write!(f, "argument {n}: {}", self.message),
        }
    }
}

/// The result of `parse_known_args`.
#[derive(Debug)]
pub(crate) enum Parsed {
    Args {
        ns: IndexMap<String, NsVal>,
        extras: Vec<String>,
    },
    /// The `version` action ran (`parser.exit()`).
    Version,
}

/// An `ArgumentParser` with `add_help=False`, `prefix_chars='-'`,
/// `allow_abbrev=True`, and `conflict_handler='resolve'`.
#[derive(Debug, Default, Clone)]
pub(crate) struct Parser {
    actions: Vec<Action>,
    /// `_option_string_actions`: option string to action index, in
    /// insertion order.
    option_map: IndexMap<String, usize>,
}

type OptionTuple = (Option<usize>, String, Option<String>, Option<String>);

fn negative_number(s: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^-\.?\d").expect("regex"))
        .is_match(s)
}

impl Parser {
    pub(crate) fn new() -> Parser {
        Parser::default()
    }

    /// `add_argument` with `_handle_conflict_resolve`.
    pub(crate) fn add(&mut self, action: Action) {
        let conflicts: Vec<String> = action
            .option_strings
            .iter()
            .filter(|o| self.option_map.contains_key(*o))
            .cloned()
            .collect();
        for option in conflicts {
            let idx = self.option_map.shift_remove(&option).expect("conflict");
            let old = &mut self.actions[idx];
            old.option_strings.retain(|o| *o != option);
            if old.option_strings.is_empty() {
                self.remove_action(idx);
            }
        }
        let idx = self.actions.len();
        for o in &action.option_strings {
            self.option_map.insert(o.clone(), idx);
        }
        self.actions.push(action);
    }

    fn remove_action(&mut self, idx: usize) {
        self.actions.remove(idx);
        for v in self.option_map.values_mut() {
            if *v > idx {
                *v -= 1;
            }
        }
    }

    /// `_parse_optional`.
    fn parse_optional(&self, arg: &str) -> Option<Vec<OptionTuple>> {
        if arg.is_empty() || !arg.starts_with('-') {
            return None;
        }
        if let Some(&idx) = self.option_map.get(arg) {
            return Some(vec![(Some(idx), arg.to_string(), None, None)]);
        }
        if arg.chars().count() == 1 {
            return None;
        }
        if let Some((opt, explicit)) = arg.split_once('=')
            && let Some(&idx) = self.option_map.get(opt)
        {
            return Some(vec![(
                Some(idx),
                opt.to_string(),
                Some("=".to_string()),
                Some(explicit.to_string()),
            )]);
        }
        let tuples = self.get_option_tuples(arg);
        if !tuples.is_empty() {
            return Some(tuples);
        }
        if negative_number(arg) {
            return None;
        }
        if arg.contains(' ') {
            return None;
        }
        Some(vec![(None, arg.to_string(), None, None)])
    }

    /// `_get_option_tuples`.
    fn get_option_tuples(&self, arg: &str) -> Vec<OptionTuple> {
        let mut result = Vec::new();
        let second = arg.chars().nth(1);
        let (prefix, sep, explicit) = match arg.split_once('=') {
            Some((p, e)) => (p, Some("=".to_string()), Some(e.to_string())),
            None => (arg, None, None),
        };
        if second == Some('-') {
            for (o, &idx) in &self.option_map {
                if o.starts_with(prefix) {
                    result.push((Some(idx), o.clone(), sep.clone(), explicit.clone()));
                }
            }
        } else {
            let split = arg.char_indices().nth(2).map_or(arg.len(), |(i, _)| i);
            let short_prefix = &arg[..split];
            let short_explicit = &arg[split..];
            for (o, &idx) in &self.option_map {
                if o == short_prefix {
                    result.push((
                        Some(idx),
                        o.clone(),
                        Some(String::new()),
                        Some(short_explicit.to_string()),
                    ));
                } else if o.starts_with(prefix) {
                    result.push((Some(idx), o.clone(), sep.clone(), explicit.clone()));
                }
            }
        }
        result
    }

    fn nargs_pattern(action: &Action) -> String {
        let option = !action.option_strings.is_empty();
        match action.nargs {
            Nargs::One => if option { "([A])" } else { "(-*A-*)" }.to_string(),
            Nargs::Optional => if option { "(A?)" } else { "(-*A?-*)" }.to_string(),
            Nargs::ZeroOrMore => if option { "(A*)" } else { "(-*[A-]*)" }.to_string(),
            Nargs::Exact(n) => {
                if option {
                    format!("([AO]{{{n}}})")
                } else {
                    format!("((?:-*A){{{n}}}-*)")
                }
            }
        }
    }

    /// `_match_argument`.
    fn match_argument(action: &Action, pattern: &str) -> Result<usize, ArgumentError> {
        let re = Regex::new(&format!("^{}", Parser::nargs_pattern(action))).expect("regex");
        match re.captures(pattern) {
            Some(c) => Ok(c.get(1).map_or(0, |m| m.len())),
            None => {
                let msg = match action.nargs {
                    Nargs::One => "expected one argument".to_string(),
                    Nargs::Optional => "expected at most one argument".to_string(),
                    Nargs::Exact(1) => "expected 1 argument".to_string(),
                    Nargs::Exact(n) => format!("expected {n} arguments"),
                    Nargs::ZeroOrMore => "expected arguments".to_string(),
                };
                Err(ArgumentError::new(Some(action), msg))
            }
        }
    }

    /// `_match_arguments_partial`.
    fn match_arguments_partial(actions: &[&Action], pattern: &str) -> Vec<usize> {
        for i in (1..=actions.len()).rev() {
            let joined: String = actions[..i]
                .iter()
                .map(|a| Parser::nargs_pattern(a))
                .collect();
            let re = Regex::new(&format!("^{joined}")).expect("regex");
            if let Some(c) = re.captures(pattern) {
                let mut result: Vec<usize> = (1..c.len())
                    .map(|g| c.get(g).map_or(0, |m| m.len()))
                    .collect();
                let end = c.get(0).map_or(0, |m| m.end());
                if end < pattern.len() && pattern.as_bytes()[end] == b'O' {
                    while result.last() == Some(&0) {
                        result.pop();
                    }
                }
                return result;
            }
        }
        Vec::new()
    }

    /// `_get_value`.
    fn get_value(action: &Action, s: &str) -> Result<NsVal, ArgumentError> {
        match action.ty {
            TypeFn::Str => Ok(NsVal::Str(s.to_string())),
            TypeFn::Int => pynum::py_int(s).map(NsVal::Int).ok_or_else(|| {
                ArgumentError::new(Some(action), format!("invalid int value: {}", str_repr(s)))
            }),
            TypeFn::Float => pynum::py_float(s).map(NsVal::Float).ok_or_else(|| {
                ArgumentError::new(
                    Some(action),
                    format!("invalid float value: {}", str_repr(s)),
                )
            }),
        }
    }

    /// `CLIArgParser._check_value` (`awscli/argparser.py`).
    fn check_value(action: &Action, value: &NsVal) -> Result<(), ArgumentError> {
        let Some(choices) = &action.choices else {
            return Ok(());
        };
        let s = value.py_str();
        if choices.contains(&s) {
            return Ok(());
        }
        let mut msg = vec![format!("Found invalid choice '{s}'\n")];
        let possible = get_close_matches(&s, choices, 3, 0.8);
        if !possible.is_empty() {
            msg.push("Maybe you meant:\n".to_string());
            for w in possible {
                msg.push(format!("  * {w}"));
            }
        }
        Err(ArgumentError::new(Some(action), msg.join("\n")))
    }

    /// `_get_values`.
    fn get_values(action: &Action, strings: &[String]) -> Result<NsVal, ArgumentError> {
        if strings.is_empty() && action.nargs == Nargs::Optional {
            let v = if action.option_strings.is_empty() {
                action.default.clone()
            } else {
                match &action.const_ {
                    Some(c) => NsVal::Str(c.clone()),
                    None => NsVal::None,
                }
            };
            return match v {
                NsVal::Str(s) => Parser::get_value(action, &s),
                other => Ok(other),
            };
        }
        if strings.is_empty()
            && action.nargs == Nargs::ZeroOrMore
            && action.option_strings.is_empty()
        {
            return Ok(match &action.default {
                NsVal::None => NsVal::List(Vec::new()),
                d => d.clone(),
            });
        }
        if strings.len() == 1 && matches!(action.nargs, Nargs::One | Nargs::Optional) {
            let v = Parser::get_value(action, &strings[0])?;
            Parser::check_value(action, &v)?;
            return Ok(v);
        }
        let mut out = Vec::new();
        for s in strings {
            out.push(Parser::get_value(action, s)?);
        }
        for v in &out {
            Parser::check_value(action, v)?;
        }
        Ok(NsVal::List(out))
    }

    /// `parse_known_args` (`_parse_known_args2` + `_parse_known_args`).
    pub(crate) fn parse_known_args(&self, args: &[String]) -> Result<Parsed, ArgumentError> {
        let mut ns: IndexMap<String, NsVal> = IndexMap::new();
        for a in &self.actions {
            if !ns.contains_key(&a.dest) {
                ns.insert(a.dest.clone(), a.default.clone());
            }
        }

        let mut option_indices: IndexMap<usize, Vec<OptionTuple>> = IndexMap::new();
        let mut pattern = String::new();
        let mut iter = args.iter().enumerate();
        while let Some((i, arg)) = iter.next() {
            if arg == "--" {
                pattern.push('-');
                for _ in iter.by_ref() {
                    pattern.push('A');
                }
                break;
            }
            match self.parse_optional(arg) {
                None => pattern.push('A'),
                Some(t) => {
                    option_indices.insert(i, t);
                    pattern.push('O');
                }
            }
        }

        let mut seen: Vec<usize> = Vec::new();
        let mut extras: Vec<String> = Vec::new();
        let mut positionals: Vec<usize> = (0..self.actions.len())
            .filter(|&i| self.actions[i].option_strings.is_empty())
            .collect();

        enum Step {
            Continue(usize),
            Version,
        }

        let take_action = |idx: usize,
                           strings: &[String],
                           ns: &mut IndexMap<String, NsVal>,
                           seen: &mut Vec<usize>|
         -> Result<bool, ArgumentError> {
            let action = &self.actions[idx];
            seen.push(idx);
            let value = Parser::get_values(action, strings)?;
            match action.kind {
                Kind::Store => {
                    ns.insert(action.dest.clone(), value);
                }
                Kind::StoreTrue => {
                    ns.insert(action.dest.clone(), NsVal::Bool(true));
                }
                Kind::StoreFalse => {
                    ns.insert(action.dest.clone(), NsVal::Bool(false));
                }
                Kind::Version => return Ok(true),
            }
            Ok(false)
        };

        let consume_optional = |start: usize,
                                ns: &mut IndexMap<String, NsVal>,
                                seen: &mut Vec<usize>,
                                extras: &mut Vec<String>|
         -> Result<Step, ArgumentError> {
            let tuples = &option_indices[&start];
            if tuples.len() > 1 {
                let options: Vec<&str> = tuples.iter().map(|t| t.1.as_str()).collect();
                return Err(ArgumentError::new(
                    None,
                    format!(
                        "ambiguous option: {} could match {}",
                        args[start],
                        options.join(", ")
                    ),
                ));
            }
            let (action, option_string, sep, explicit) = tuples[0].clone();
            let Some(mut idx) = action else {
                extras.push(args[start].clone());
                return Ok(Step::Continue(start + 1));
            };
            let mut option_string = option_string;
            let mut sep = sep;
            let mut explicit = explicit;
            let mut action_tuples: Vec<(usize, Vec<String>)> = Vec::new();
            let stop;
            loop {
                let action = &self.actions[idx];
                if let Some(exp) = explicit.clone() {
                    let arg_count = Parser::match_argument(action, "A")?;
                    let second = option_string.chars().nth(1);
                    if arg_count == 0 && second != Some('-') && !exp.is_empty() {
                        if sep.is_some_and(|s| !s.is_empty()) || exp.starts_with('-') {
                            return Err(ArgumentError::new(
                                Some(action),
                                format!("ignored explicit argument {}", str_repr(&exp)),
                            ));
                        }
                        action_tuples.push((idx, Vec::new()));
                        let first = exp.chars().next().expect("explicit");
                        let new_option = format!("-{first}");
                        if let Some(&next) = self.option_map.get(&new_option) {
                            idx = next;
                            option_string = new_option;
                            let rest = &exp[first.len_utf8()..];
                            if rest.is_empty() {
                                sep = None;
                                explicit = None;
                            } else if let Some(r) = rest.strip_prefix('=') {
                                sep = Some("=".to_string());
                                explicit = Some(r.to_string());
                            } else {
                                sep = Some(String::new());
                                explicit = Some(rest.to_string());
                            }
                        } else {
                            extras.push(format!("-{exp}"));
                            stop = start + 1;
                            break;
                        }
                    } else if arg_count == 1 {
                        stop = start + 1;
                        action_tuples.push((idx, vec![exp]));
                        break;
                    } else {
                        return Err(ArgumentError::new(
                            Some(action),
                            format!("ignored explicit argument {}", str_repr(&exp)),
                        ));
                    }
                } else {
                    let s = start + 1;
                    let arg_count = Parser::match_argument(action, &pattern[s..])?;
                    stop = s + arg_count;
                    action_tuples.push((idx, args[s..stop].to_vec()));
                    break;
                }
            }
            for (idx, strings) in action_tuples {
                if take_action(idx, &strings, ns, seen)? {
                    return Ok(Step::Version);
                }
            }
            Ok(Step::Continue(stop))
        };

        let consume_positionals = |start: usize,
                                   positionals: &mut Vec<usize>,
                                   ns: &mut IndexMap<String, NsVal>,
                                   seen: &mut Vec<usize>|
         -> Result<usize, ArgumentError> {
            let acts: Vec<&Action> = positionals.iter().map(|&i| &self.actions[i]).collect();
            let counts = Parser::match_arguments_partial(&acts, &pattern[start..]);
            let mut start = start;
            for (k, &count) in counts.iter().enumerate() {
                let idx = positionals[k];
                let mut strings = args[start..start + count].to_vec();
                if pattern[start..start + count].contains('-')
                    && let Some(p) = strings.iter().position(|s| s == "--")
                {
                    strings.remove(p);
                }
                start += count;
                take_action(idx, &strings, ns, seen)?;
            }
            positionals.drain(..counts.len());
            Ok(start)
        };

        let max_option = option_indices.keys().copied().max();
        let mut start = 0usize;
        if let Some(max_option) = max_option {
            while start <= max_option {
                let mut next = start;
                while next <= max_option {
                    if option_indices.contains_key(&next) {
                        break;
                    }
                    next += 1;
                }
                if start != next {
                    let end = consume_positionals(start, &mut positionals, &mut ns, &mut seen)?;
                    if end > start {
                        start = end;
                        continue;
                    }
                    start = end;
                }
                if !option_indices.contains_key(&start) {
                    extras.extend_from_slice(&args[start..next]);
                    start = next;
                }
                match consume_optional(start, &mut ns, &mut seen, &mut extras)? {
                    Step::Continue(s) => start = s,
                    Step::Version => return Ok(Parsed::Version),
                }
            }
        }
        let stop = consume_positionals(start, &mut positionals, &mut ns, &mut seen)?;
        extras.extend_from_slice(&args[stop..]);

        let required: Vec<String> = self
            .actions
            .iter()
            .enumerate()
            .filter(|(i, a)| !seen.contains(i) && a.required)
            .filter_map(|(_, a)| a.name())
            .collect();
        if !required.is_empty() {
            return Err(ArgumentError::new(
                None,
                format!(
                    "the following arguments are required: {}",
                    required.join(", ")
                ),
            ));
        }
        Ok(Parsed::Args { ns, extras })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn abbreviations_and_extras() {
        let mut p = Parser::new();
        p.add(Action::option("--region"));
        p.add(Action::option("--output"));
        p.add(Action::flag("--no-paginate", Kind::StoreFalse));
        let mut cmd = Action::positional("command", Nargs::One);
        cmd.choices = Some(s(&["ec2", "s3"]));
        p.add(cmd);
        let Parsed::Args { ns, extras } = p
            .parse_known_args(&s(&["ec2", "run", "--reg", "x", "--foo", "y"]))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(ns["region"], NsVal::Str("x".into()));
        assert_eq!(ns["command"], NsVal::Str("ec2".into()));
        assert_eq!(extras, s(&["run", "--foo", "y"]));
    }

    #[test]
    fn expected_one_argument() {
        let mut p = Parser::new();
        p.add(Action::option("--region"));
        let e = p.parse_known_args(&s(&["--region"])).unwrap_err();
        assert_eq!(e.to_string(), "argument --region: expected one argument");
    }
}
