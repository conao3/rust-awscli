//! The reStructuredText state machine (`docutils/parsers/rst/states.py`,
//! `docutils/statemachine.py` `StateMachine.run` and friends) and the
//! document's `Reporter` (`docutils/utils/__init__.py`).

use std::sync::OnceLock;

use super::chars::{clen, column_width, rstrip, slice, slice_from, slice_to, split_ws, strip};
use super::nodes::{Doc, Id, Tag, Val, fully_normalize_name, unescape, whitespace_normalize_name};
use super::pyre::{Match, Regex};
use super::statemachine::{Info, StringList, py_index};

/// Control flow the Python code expresses with exceptions.
#[derive(Debug)]
pub(super) enum Flow {
    /// `EOFError`.
    Eof,
    /// `TransitionCorrection(transition)`.
    TransCorr(&'static str),
    /// `StateCorrection(state, transition)`.
    StateCorr(St, Option<&'static str>),
    /// `SystemMessage` (a message at or above the halt level).
    Halt(String),
    /// `MarkupError`.
    Markup(String),
}

pub(super) type R<T> = Result<T, Flow>;

/// The `State` classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum St {
    Body,
    BulletList,
    DefinitionList,
    EnumeratedList,
    FieldList,
    OptionList,
    LineBlock,
    ExtensionOptions,
    Explicit,
    Text,
    Definition,
    Line,
    SubstitutionDef,
    QuotedLiteralBlock,
    MetaBody,
}

const BODY_TRANSITIONS: &[&str] = &[
    "blank",
    "indent",
    "bullet",
    "enumerator",
    "field_marker",
    "option_marker",
    "doctest",
    "line_block",
    "grid_table_top",
    "simple_table_top",
    "explicit_markup",
    "anonymous",
    "line",
    "text",
];
const TEXT_TRANSITIONS: &[&str] = &["blank", "indent", "underline", "text"];
const SUBDEF_TRANSITIONS: &[&str] = &["blank", "indent", "embedded_directive", "text"];
const QLB_INITIAL: &[&str] = &["blank", "indent", "initial_quoted", "text"];
const QLB_QUOTED: &[&str] = &["quoted", "blank", "indent", "text"];

pub(super) const NONALPHANUM7BIT: &str = "[!-/:-@[-`{-~]";
pub(super) const SIMPLENAME: &str = r"(?:(?!_)\w)+(?:[-._+:](?:(?!_)\w)+)*";

/// The compiled patterns of `Body`, `Text`, `Inliner` and `Explicit`.
pub(super) struct Patterns {
    pub blank: Regex,
    pub indent: Regex,
    pub bullet: Regex,
    pub enumerator: Regex,
    pub field_marker: Regex,
    pub option_marker: Regex,
    pub doctest: Regex,
    pub line_block: Regex,
    pub grid_table_top: Regex,
    pub simple_table_top: Regex,
    pub simple_table_border: Regex,
    pub explicit_markup: Regex,
    pub anonymous: Regex,
    pub line: Regex,
    pub embedded_directive: Regex,
    pub initial_quoted: Regex,
    pub sequences: Vec<(&'static str, Regex)>,
    pub ex_target: Regex,
    pub ex_reference: Regex,
    pub ex_substitution: Regex,
    pub ex_footnote: Regex,
    pub ex_citation: Regex,
    pub ex_hyperlink_target: Regex,
    pub ex_substitution_def: Regex,
    pub ex_directive: Regex,
    pub attribution: Regex,
    pub literal_marker: Regex,
    pub inline: super::inliner::InlinePatterns,
}

pub(super) fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let enum_pat = "([0-9]+|[a-z]|[A-Z]|[ivxlcdm]+|[IVXLCDM]+|#)";
        let alphanum = "[a-zA-Z0-9]";
        let alphanumplus = "[a-zA-Z0-9_-]";
        let optname = format!("{alphanum}{alphanumplus}*");
        let optarg = format!("([a-zA-Z]{alphanumplus}*|<[^<>]+>)");
        let shortopt = format!(r"(-|\+){alphanum}( ?{optarg})?");
        let longopt = format!(r"(--|/){optname}([ =]{optarg})?");
        let option = format!("({shortopt}|{longopt})");
        let parens = format!(r"(?P<parens>\({enum_pat}\))");
        let rparen = format!(r"(?P<rparen>{enum_pat}\))");
        let period = format!(r"(?P<period>{enum_pat}\.)");
        let ws_before = r"(?<![\s\x00])";
        Patterns {
            blank: Regex::new(" *$", false),
            indent: Regex::new(" +", false),
            bullet: Regex::new("[-+*\u{2022}\u{2023}\u{2043}]( +|$)", false),
            enumerator: Regex::new(&format!("({parens}|{rparen}|{period})( +|$)"), false),
            field_marker: Regex::new(r":(?![: ])([^:\\]|\\.|:(?!([ `]|$)))*(?<! ):( +|$)", false),
            option_marker: Regex::new(&format!("{option}(, {option})*(  +| ?$)"), false),
            doctest: Regex::new(">>>( +|$)", false),
            line_block: Regex::new(r"\|( +|$)", false),
            grid_table_top: Regex::new(r"\+-[-+]+-\+ *$", false),
            simple_table_top: Regex::new("=+( +=+)+ *$", false),
            simple_table_border: Regex::new("=+[ =]*$", false),
            explicit_markup: Regex::new(r"\.\.( +|$)", false),
            anonymous: Regex::new("__( +|$)", false),
            line: Regex::new(&format!(r"({NONALPHANUM7BIT})\1* *$"), false),
            embedded_directive: Regex::new(&format!("({SIMPLENAME})::( +|$)"), false),
            initial_quoted: Regex::new(&format!("({NONALPHANUM7BIT})"), false),
            sequences: vec![
                ("arabic", Regex::new("[0-9]+$", false)),
                ("loweralpha", Regex::new("[a-z]$", false)),
                ("upperalpha", Regex::new("[A-Z]$", false)),
                ("lowerroman", Regex::new("[ivxlcdm]+$", false)),
                ("upperroman", Regex::new("[IVXLCDM]+$", false)),
            ],
            ex_target: Regex::new(
                &format!(
                    r"(_|(?!_)(?P<quote>`?)(?![ `])(?P<name>.+?){ws_before}(?P=quote))(?<!(?<!\x00):){ws_before}[ ]?:([ ]+|$)"
                ),
                false,
            ),
            ex_reference: Regex::new(
                &format!(r"((?P<simple>{SIMPLENAME})_|`(?![ ])(?P<phrase>.+?){ws_before}`_)$"),
                false,
            ),
            ex_substitution: Regex::new(
                &format!(r"((?![ ])(?P<name>.+?){ws_before}\|)([ ]+|$)"),
                false,
            ),
            ex_footnote: Regex::new(
                &format!(r"\.\.[ ]+\[([0-9]+|\#|\#{SIMPLENAME}|\*)\]([ ]+|$)"),
                false,
            ),
            ex_citation: Regex::new(&format!(r"\.\.[ ]+\[({SIMPLENAME})\]([ ]+|$)"), false),
            ex_hyperlink_target: Regex::new(r"\.\.[ ]+_(?![ ]|$)", false),
            ex_substitution_def: Regex::new(r"\.\.[ ]+\|(?![ ]|$)", false),
            ex_directive: Regex::new(&format!(r"\.\.[ ]+({SIMPLENAME})[ ]?::([ ]+|$)"), false),
            attribution: Regex::new("(---?(?!-)|\u{2014}) *(?=[^ \\n])", false),
            literal_marker: Regex::new(r"(?<!\\)(\\\\)*::$", false),
            inline: super::inliner::InlinePatterns::new(),
        }
    })
}

/// `memo`: parse-global data shared by the nested state machines.
pub(super) struct Memo {
    pub title_styles: Vec<String>,
    pub section_level: usize,
    pub section_bubble_up_kludge: bool,
}

/// One `StateMachine` run (its input, position and the state objects'
/// attributes).
pub(super) struct Sm {
    pub lines: StringList,
    pub input_offset: usize,
    pub line_offset: isize,
    pub line: Option<String>,
    pub match_titles: bool,
    pub node: Id,
    pub is_top: bool,
    pub running: bool,
    pub blank_finish: Vec<(St, bool)>,
    pub lastordinal: i64,
    pub format: String,
    pub auto: bool,
    pub eofcheck: bool,
    pub qlb_quote: Option<char>,
    pub qlb_messages: Vec<Id>,
    pub qlb_initial_lineno: Option<isize>,
}

impl Sm {
    pub(super) fn new(lines: StringList, input_offset: usize, node: Id, match_titles: bool) -> Sm {
        Sm {
            lines,
            input_offset,
            line_offset: -1,
            line: None,
            match_titles,
            node,
            is_top: false,
            running: false,
            blank_finish: Vec::new(),
            lastordinal: 0,
            format: String::new(),
            auto: false,
            eofcheck: true,
            qlb_quote: None,
            qlb_messages: Vec::new(),
            qlb_initial_lineno: None,
        }
    }
    pub(super) fn abs_line_offset(&self) -> isize {
        self.line_offset + self.input_offset as isize
    }
    pub(super) fn abs_line_number(&self) -> isize {
        self.abs_line_offset() + 1
    }
    pub(super) fn cur(&self) -> String {
        self.line.clone().unwrap_or_default()
    }
    pub(super) fn get_bf(&self, st: St) -> bool {
        self.blank_finish
            .iter()
            .find(|(s, _)| *s == st)
            .is_some_and(|(_, b)| *b)
    }
    pub(super) fn set_bf(&mut self, st: St, v: bool) {
        if let Some(e) = self.blank_finish.iter_mut().find(|(s, _)| *s == st) {
            e.1 = v;
        } else {
            self.blank_finish.push((st, v));
        }
    }
    /// `StateMachine.is_next_line_blank`.
    pub(super) fn is_next_line_blank(&self) -> bool {
        match py_index(self.lines.len(), self.line_offset + 1) {
            Some(i) => strip(&self.lines.data[i]).is_empty(),
            None => true,
        }
    }
    /// `StateMachine.at_eof`.
    pub(super) fn at_eof(&self) -> bool {
        self.line_offset >= self.lines.len() as isize - 1
    }
    /// `StateMachine.get_source_and_line`.
    pub(super) fn get_source_and_line(
        &self,
        lineno: Option<isize>,
    ) -> (Option<String>, Option<usize>) {
        source_and_line(&self.lines, self.input_offset, self.line_offset, lineno)
    }
}

/// `StateMachine.get_source_and_line` for a machine's state.
pub(super) fn source_and_line(
    lines: &StringList,
    input_offset: usize,
    line_offset: isize,
    lineno: Option<isize>,
) -> (Option<String>, Option<usize>) {
    let offset = match lineno {
        None => line_offset,
        Some(l) => l - input_offset as isize - 1,
    };
    match lines.info(offset) {
        Info::Offset(o) => (Some("<string>".into()), Some(o + 1)),
        Info::PastEnd => {
            let (s, l) = source_and_line(
                lines,
                input_offset,
                line_offset,
                Some(offset + input_offset as isize),
            );
            (s, l.map(|l| l + 1))
        }
        Info::Missing => (None, None),
    }
}

/// Keyword arguments of `Reporter.system_message`.
#[derive(Default)]
pub(super) struct MsgArgs {
    pub line: Option<isize>,
    pub source: Option<String>,
    pub base_node: Option<Id>,
    pub backrefs: Vec<String>,
    pub children: Vec<Id>,
}

impl MsgArgs {
    pub(super) fn line(l: isize) -> MsgArgs {
        MsgArgs {
            line: Some(l),
            ..Default::default()
        }
    }
    pub(super) fn base(n: Id) -> MsgArgs {
        MsgArgs {
            base_node: Some(n),
            ..Default::default()
        }
    }
    pub(super) fn child(mut self, c: Id) -> MsgArgs {
        self.children.push(c);
        self
    }
}

/// The parser: document, reporter and the parse-global state.
pub(super) struct P {
    pub doc: Doc,
    pub stderr: String,
    pub top: Option<(StringList, isize)>,
    pub memo: Memo,
    pub pats: &'static Patterns,
}

const LEVELS: [&str; 5] = ["DEBUG", "INFO", "WARNING", "ERROR", "SEVERE"];

impl P {
    pub(super) fn new() -> P {
        P {
            doc: Doc::new(),
            stderr: String::new(),
            top: None,
            memo: Memo {
                title_styles: Vec::new(),
                section_level: 0,
                section_bubble_up_kludge: false,
            },
            pats: patterns(),
        }
    }

    /// The reporter's `get_source_and_line` (bound to the top-level state
    /// machine).
    fn reporter_source_line(&self, lineno: Option<isize>) -> (Option<String>, Option<usize>) {
        match &self.top {
            None => (None, None),
            Some((lines, off)) => source_and_line(lines, 0, *off, lineno),
        }
    }

    /// `Reporter.system_message`.
    pub(super) fn system_message(&mut self, level: usize, message: &str, args: MsgArgs) -> R<Id> {
        let mut source: Option<String> = args.source.clone();
        let mut line: Option<isize> = args.line;
        if let Some(b) = args.base_node {
            let (s, l) = self.doc.source_line(b);
            if source.is_none()
                && let Some(s) = s
            {
                source = Some(s);
            }
            if line.is_none()
                && let Some(l) = l
            {
                line = Some(l as isize);
            }
        }
        if source.is_none() {
            let (s, l) = self.reporter_source_line(line);
            if s.is_some() {
                source = s;
            }
            if let Some(l) = l {
                line = Some(l as isize);
            }
        }
        let source = source.unwrap_or_else(|| "<string>".to_string());
        let msg = self.doc.elem(Tag::SystemMessage, "");
        if !message.is_empty() {
            let p = self.doc.text_elem(Tag::Paragraph, "", message);
            self.doc.append(msg, p);
        }
        for &c in &args.children {
            self.doc.append(msg, c);
        }
        self.doc.set(msg, "level", Val::Int(level as i64));
        self.doc.set_str(msg, "type", LEVELS[level]);
        self.doc.set_str(msg, "source", &source);
        if let Some(l) = line {
            self.doc.set(msg, "line", Val::Int(l as i64));
        }
        if !args.backrefs.is_empty() {
            self.doc
                .set(msg, "backrefs", Val::List(args.backrefs.clone()));
        }
        if level >= 2 {
            let t = self.doc.astext(msg);
            self.stderr.push_str(&t);
            self.stderr.push('\n');
        }
        if level >= 4 {
            return Err(Flow::Halt(self.doc.astext(msg)));
        }
        if level > 0 && self.doc.observe_transform {
            self.doc.transform_messages.push(msg);
        }
        Ok(msg)
    }
    pub(super) fn info(&mut self, m: &str, a: MsgArgs) -> R<Id> {
        self.system_message(1, m, a)
    }
    pub(super) fn warning(&mut self, m: &str, a: MsgArgs) -> R<Id> {
        self.system_message(2, m, a)
    }
    pub(super) fn error(&mut self, m: &str, a: MsgArgs) -> R<Id> {
        self.system_message(3, m, a)
    }
    pub(super) fn severe(&mut self, m: &str, a: MsgArgs) -> R<Id> {
        self.system_message(4, m, a)
    }

    /// A `literal_block(text, text)`.
    pub(super) fn literal(&mut self, text: &str) -> Id {
        self.doc.text_elem(Tag::LiteralBlock, text, text)
    }

    fn notify(&mut self, sm: &Sm) {
        if sm.is_top
            && let Some(t) = &mut self.top
        {
            t.1 = sm.line_offset;
        }
        if sm.running {
            match sm.lines.info(sm.line_offset) {
                Info::Offset(o) => {
                    self.doc.current_source = Some("<string>".into());
                    self.doc.current_line = Some(o + 1);
                }
                Info::PastEnd => {
                    self.doc.current_source = Some("<string>".into());
                    self.doc.current_line = None;
                }
                Info::Missing => {
                    self.doc.current_source = None;
                    self.doc.current_line = None;
                }
            }
        }
    }

    /// `StateMachine.next_line`.
    pub(super) fn next_line(&mut self, sm: &mut Sm, n: isize) -> R<String> {
        sm.line_offset += n;
        let r = match py_index(sm.lines.len(), sm.line_offset) {
            Some(i) => {
                sm.line = Some(sm.lines.data[i].clone());
                Ok(sm.lines.data[i].clone())
            }
            None => {
                sm.line = None;
                Err(Flow::Eof)
            }
        };
        self.notify(sm);
        r
    }

    /// `StateMachine.previous_line`.
    pub(super) fn previous_line(&mut self, sm: &mut Sm, n: isize) {
        sm.line_offset -= n;
        if sm.line_offset < 0 {
            sm.line = None;
        } else {
            sm.line = py_index(sm.lines.len(), sm.line_offset).map(|i| sm.lines.data[i].clone());
        }
        self.notify(sm);
    }

    /// `RSTState.goto_line` (`StateMachine.goto_line` ignoring EOF).
    pub(super) fn goto_line(&mut self, sm: &mut Sm, abs_line_offset: isize) {
        sm.line_offset = abs_line_offset - sm.input_offset as isize;
        sm.line = py_index(sm.lines.len(), sm.line_offset).map(|i| sm.lines.data[i].clone());
        self.notify(sm);
    }

    /// `StateMachine.get_text_block`.
    pub(super) fn get_text_block(
        &mut self,
        sm: &mut Sm,
        flush_left: bool,
    ) -> R<Result<StringList, (StringList, Option<usize>)>> {
        let start = sm.line_offset.max(0) as usize;
        match sm.lines.get_text_block(start, flush_left) {
            Ok(b) => {
                self.next_line(sm, b.len() as isize - 1)?;
                Ok(Ok(b))
            }
            Err((b, l)) => {
                self.next_line(sm, b.len() as isize - 1)?;
                Ok(Err((b, l)))
            }
        }
    }

    /// `StateMachineWS.get_indented`.
    pub(super) fn get_indented(
        &mut self,
        sm: &mut Sm,
        until_blank: bool,
        strip_indent: bool,
    ) -> R<(StringList, usize, isize, bool)> {
        let mut offset = sm.abs_line_offset();
        let (mut indented, indent, bf) = sm.lines.get_indented(
            sm.line_offset.max(0) as usize,
            until_blank,
            strip_indent,
            None,
            None,
        );
        if !indented.is_empty() {
            self.next_line(sm, indented.len() as isize - 1)?;
        }
        while !indented.is_empty() && strip(&indented.data[0]).is_empty() {
            indented.trim_start(1);
            offset += 1;
        }
        Ok((indented, indent, offset, bf))
    }

    /// `StateMachineWS.get_known_indented`.
    pub(super) fn get_known_indented(
        &mut self,
        sm: &mut Sm,
        indent: usize,
    ) -> R<(StringList, isize, bool)> {
        let mut offset = sm.abs_line_offset();
        let (mut indented, _, bf) = sm.lines.get_indented(
            sm.line_offset.max(0) as usize,
            false,
            true,
            Some(indent),
            None,
        );
        self.next_line(sm, indented.len() as isize - 1)?;
        while !indented.is_empty() && strip(&indented.data[0]).is_empty() {
            indented.trim_start(1);
            offset += 1;
        }
        Ok((indented, offset, bf))
    }

    /// `StateMachineWS.get_first_known_indented`.
    pub(super) fn get_first_known_indented(
        &mut self,
        sm: &mut Sm,
        indent: usize,
        until_blank: bool,
        strip_indent: bool,
        strip_top: bool,
    ) -> R<(StringList, usize, isize, bool)> {
        let mut offset = sm.abs_line_offset();
        let (mut indented, ind, bf) = sm.lines.get_indented(
            sm.line_offset.max(0) as usize,
            until_blank,
            strip_indent,
            None,
            Some(indent),
        );
        self.next_line(sm, indented.len() as isize - 1)?;
        if strip_top {
            while !indented.is_empty() && strip(&indented.data[0]).is_empty() {
                indented.trim_start(1);
                offset += 1;
            }
        }
        Ok((indented, ind, offset, bf))
    }

    /// The transition names of a state, in matching order.
    fn transition_order(sm: &Sm, st: St) -> &'static [&'static str] {
        match st {
            St::Text | St::Definition | St::Line => TEXT_TRANSITIONS,
            St::SubstitutionDef => SUBDEF_TRANSITIONS,
            St::QuotedLiteralBlock => {
                if sm.qlb_quote.is_some() {
                    QLB_QUOTED
                } else {
                    QLB_INITIAL
                }
            }
            _ => BODY_TRANSITIONS,
        }
    }

    fn transition_match(&self, sm: &Sm, st: St, name: &str, line: &str) -> Option<Match> {
        let p = self.pats;
        let re = match (st, name) {
            (_, "blank") => &p.blank,
            (_, "indent") => &p.indent,
            (St::Text | St::Definition | St::Line, "underline") => &p.line,
            (_, "text") => return Regex::new("", false).match_str(line),
            (_, "embedded_directive") => &p.embedded_directive,
            (_, "initial_quoted") => &p.initial_quoted,
            (_, "quoted") => {
                let q = sm.qlb_quote.unwrap().to_string();
                return Regex::new(&regex_escape(&q), false).match_str(line);
            }
            (_, "bullet") => &p.bullet,
            (_, "enumerator") => &p.enumerator,
            (_, "field_marker") => &p.field_marker,
            (_, "option_marker") => &p.option_marker,
            (_, "doctest") => &p.doctest,
            (_, "line_block") => &p.line_block,
            (_, "grid_table_top") => &p.grid_table_top,
            (_, "simple_table_top") => &p.simple_table_top,
            (_, "explicit_markup") => &p.explicit_markup,
            (_, "anonymous") => &p.anonymous,
            (_, "line") => &p.line,
            _ => unreachable!("transition {name}"),
        };
        re.match_str(line)
    }

    fn next_state_of(st: St, name: &str) -> St {
        match (st, name) {
            (St::Text | St::Definition | St::Line, "underline" | "text") => St::Body,
            _ => st,
        }
    }

    /// `StateMachine.run`.
    pub(super) fn run(&mut self, sm: &mut Sm, initial: St) -> R<()> {
        sm.line_offset = -1;
        sm.running = true;
        let mut state = initial;
        let mut context: Vec<String> = Vec::new();
        let mut transitions: Option<&'static str> = None;
        loop {
            let r = match self.next_line(sm, 1) {
                Ok(_) => self.check_line(sm, state, &mut context, transitions),
                Err(e) => Err(e),
            };
            match r {
                Ok(next) => {
                    transitions = None;
                    state = next;
                }
                Err(Flow::Eof) => {
                    let r = self.eof(sm, state, &mut context);
                    sm.running = false;
                    r?;
                    break;
                }
                Err(Flow::TransCorr(name)) => {
                    self.previous_line(sm, 1);
                    transitions = Some(name);
                }
                Err(Flow::StateCorr(st, name)) => {
                    self.previous_line(sm, 1);
                    state = st;
                    transitions = name;
                }
                Err(e) => {
                    sm.running = false;
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn check_line(
        &mut self,
        sm: &mut Sm,
        st: St,
        context: &mut Vec<String>,
        only: Option<&'static str>,
    ) -> R<St> {
        let line = sm.cur();
        let order: Vec<&'static str> = match only {
            Some(n) => vec![n],
            None => Self::transition_order(sm, st).to_vec(),
        };
        for name in order {
            if let Some(m) = self.transition_match(sm, st, name, &line) {
                let next = Self::next_state_of(st, name);
                return self.dispatch(sm, st, name, m, context, next);
            }
        }
        Ok(st)
    }

    fn specialized_body(st: St) -> bool {
        !matches!(
            st,
            St::Body
                | St::Text
                | St::Definition
                | St::Line
                | St::SubstitutionDef
                | St::QuotedLiteralBlock
        )
    }

    /// `SpecializedBody.invalid_input`.
    fn invalid_input<T>(&mut self, sm: &mut Sm) -> R<T> {
        self.previous_line(sm, 1);
        Err(Flow::Eof)
    }

    fn dispatch(
        &mut self,
        sm: &mut Sm,
        st: St,
        name: &'static str,
        m: Match,
        context: &mut Vec<String>,
        next: St,
    ) -> R<St> {
        match st {
            St::Text | St::Definition | St::Line => {
                return self.text_dispatch(sm, st, name, m, context);
            }
            St::QuotedLiteralBlock => return self.qlb_dispatch(sm, name, m, context, next),
            St::SubstitutionDef => {
                return match name {
                    "blank" => Ok(next),
                    "indent" => self.body_indent(sm, next),
                    "embedded_directive" => {
                        let alt = self.doc.list(sm.node, "names").first().cloned();
                        let (nodes, bf) = self.directive(sm, &m, alt)?;
                        let node = sm.node;
                        self.doc.extend(node, &nodes);
                        if !sm.at_eof() {
                            sm.set_bf(St::SubstitutionDef, bf);
                        }
                        Err(Flow::Eof)
                    }
                    _ => {
                        if !sm.at_eof() {
                            let b = sm.is_next_line_blank();
                            sm.set_bf(St::SubstitutionDef, b);
                        }
                        Err(Flow::Eof)
                    }
                };
            }
            _ => {}
        }
        let special = Self::specialized_body(st);
        match name {
            "blank" => {
                if matches!(st, St::LineBlock | St::Explicit) {
                    self.invalid_input(sm)
                } else {
                    Ok(next)
                }
            }
            "indent" if special => self.invalid_input(sm),
            "indent" => self.body_indent(sm, next),
            "bullet" => match st {
                St::Body => self.bullet(sm, &m, next),
                St::BulletList => {
                    let bullet = self.doc.get_str(sm.node, "bullet").unwrap_or_default();
                    if !sm.cur().starts_with(&bullet) {
                        return self.invalid_input(sm);
                    }
                    let (item, bf) = self.list_item(sm, m.end())?;
                    let node = sm.node;
                    self.doc.append(node, item);
                    sm.set_bf(st, bf);
                    Ok(next)
                }
                _ => self.invalid_input(sm),
            },
            "enumerator" => match st {
                St::Body => self.enumerator(sm, &m, next),
                St::EnumeratedList => self.enumerated_list_item(sm, &m, next),
                _ => self.invalid_input(sm),
            },
            "field_marker" => match st {
                St::Body => self.field_marker(sm, &m, next),
                St::FieldList | St::ExtensionOptions => {
                    let (field, bf) = self.field(sm, &m, st == St::ExtensionOptions)?;
                    let node = sm.node;
                    self.doc.append(node, field);
                    sm.set_bf(st, bf);
                    Ok(next)
                }
                St::MetaBody => {
                    let node = self.parsemeta(sm, &m)?;
                    let parent = sm.node;
                    self.doc.append(parent, node);
                    Ok(next)
                }
                _ => self.invalid_input(sm),
            },
            "option_marker" => match st {
                St::Body => self.option_marker(sm, &m, next),
                St::OptionList => match self.option_list_item(sm, &m) {
                    Ok((item, bf)) => {
                        let node = sm.node;
                        self.doc.append(node, item);
                        sm.set_bf(st, bf);
                        Ok(next)
                    }
                    Err(Flow::Markup(_)) => self.invalid_input(sm),
                    Err(e) => Err(e),
                },
                _ => self.invalid_input(sm),
            },
            "doctest" if special => self.invalid_input(sm),
            "doctest" => {
                let block = match self.get_text_block(sm, false)? {
                    Ok(b) | Err((b, _)) => b,
                };
                let data = block.data.join("\n");
                let n = self.doc.text_elem(Tag::DoctestBlock, &data, &data);
                let parent = sm.node;
                self.doc.append(parent, n);
                Ok(next)
            }
            "line_block" => match st {
                St::Body => self.line_block(sm, &m, next),
                St::LineBlock => {
                    let lineno = sm.abs_line_number();
                    let (line, messages, bf) = self.line_block_line(sm, &m, lineno)?;
                    let node = sm.node;
                    self.doc.append(node, line);
                    let pp = self.doc.parent(node).expect("line block parent");
                    self.doc.extend(pp, &messages);
                    sm.set_bf(st, bf);
                    Ok(next)
                }
                _ => self.invalid_input(sm),
            },
            "grid_table_top" if special => self.invalid_input(sm),
            "grid_table_top" => self.table_top(sm, next, true),
            "simple_table_top" if special => self.invalid_input(sm),
            "simple_table_top" => self.table_top(sm, next, false),
            "explicit_markup" => match st {
                St::Body => {
                    let (nodes, bf) = self.explicit_construct(sm, &m)?;
                    let parent = sm.node;
                    self.doc.extend(parent, &nodes);
                    self.explicit_list(sm, bf)?;
                    Ok(next)
                }
                St::Explicit => {
                    let (nodes, bf) = self.explicit_construct(sm, &m)?;
                    let parent = sm.node;
                    self.doc.extend(parent, &nodes);
                    sm.set_bf(st, bf);
                    Ok(next)
                }
                _ => self.invalid_input(sm),
            },
            "anonymous" => match st {
                St::Body => {
                    let (nodes, bf) = self.anonymous_target(sm, &m)?;
                    let parent = sm.node;
                    self.doc.extend(parent, &nodes);
                    self.explicit_list(sm, bf)?;
                    Ok(next)
                }
                St::Explicit => {
                    let (nodes, bf) = self.anonymous_target(sm, &m)?;
                    let parent = sm.node;
                    self.doc.extend(parent, &nodes);
                    sm.set_bf(st, bf);
                    Ok(next)
                }
                _ => self.invalid_input(sm),
            },
            "line" if special => self.invalid_input(sm),
            "line" => self.body_line(sm, &m, next, context),
            "text" => match st {
                St::Body => {
                    *context = vec![sm.cur()];
                    Ok(St::Text)
                }
                St::DefinitionList => {
                    *context = vec![sm.cur()];
                    Ok(St::Definition)
                }
                _ => self.invalid_input(sm),
            },
            _ => unreachable!("transition {name}"),
        }
    }

    fn eof(&mut self, sm: &mut Sm, st: St, context: &mut Vec<String>) -> R<()> {
        match st {
            St::Text => {
                if !context.is_empty() {
                    self.text_blank(sm, context)?;
                }
                Ok(())
            }
            St::Definition => {
                self.previous_line(sm, 2);
                Ok(())
            }
            St::Line => self.line_eof(sm, context),
            St::QuotedLiteralBlock => self.qlb_eof(sm, context),
            _ => Ok(()),
        }
    }

    /// `RSTState.nested_parse`.
    pub(super) fn nested_parse(
        &mut self,
        block: StringList,
        input_offset: isize,
        node: Id,
        match_titles: bool,
    ) -> R<isize> {
        let mut sm = Sm::new(block, input_offset as usize, node, match_titles);
        self.run(&mut sm, St::Body)?;
        Ok(sm.abs_line_offset())
    }

    /// `RSTState.nested_list_parse`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn nested_list_parse(
        &mut self,
        block: StringList,
        input_offset: isize,
        node: Id,
        initial: St,
        blank_finish: bool,
        blank_finish_state: Option<St>,
        match_titles: bool,
        setup: &dyn Fn(&mut Sm),
    ) -> R<(isize, bool)> {
        let mut sm = Sm::new(block, input_offset as usize, node, match_titles);
        let bfs = blank_finish_state.unwrap_or(initial);
        sm.set_bf(bfs, blank_finish);
        setup(&mut sm);
        self.run(&mut sm, initial)?;
        Ok((sm.abs_line_offset(), sm.get_bf(bfs)))
    }

    /// The rest of the current machine's input after the current line.
    fn rest(sm: &Sm) -> StringList {
        sm.lines.slice_from((sm.line_offset + 1).max(0) as usize)
    }

    /// `RSTState.unindent_warning`.
    pub(super) fn unindent_warning(&mut self, sm: &Sm, node_name: &str) -> R<Id> {
        let lineno = sm.abs_line_number() + 1;
        self.warning(
            &format!("{node_name} ends without a blank line; unexpected unindent."),
            MsgArgs::line(lineno),
        )
    }

    fn add(&mut self, parent: Id, child: Id) {
        self.doc.append(parent, child);
    }

    /// `RSTState.check_subsection`.
    fn check_subsection(
        &mut self,
        sm: &mut Sm,
        source: &str,
        style: &str,
        lineno: isize,
    ) -> R<bool> {
        let mylevel = self.memo.section_level;
        let level = match self.memo.title_styles.iter().position(|s| s == style) {
            Some(i) => i + 1,
            None => {
                if self.memo.title_styles.len() == self.memo.section_level {
                    self.memo.title_styles.push(style.to_string());
                    return Ok(true);
                }
                let e = self.title_inconsistent(source, lineno)?;
                self.add(sm.node, e);
                return Ok(false);
            }
        };
        if level <= mylevel {
            self.memo.section_level = level;
            if clen(style) == 2 {
                self.memo.section_bubble_up_kludge = true;
            }
            self.previous_line(sm, clen(style) as isize + 1);
            return Err(Flow::Eof);
        }
        if level == mylevel + 1 {
            return Ok(true);
        }
        let e = self.title_inconsistent(source, lineno)?;
        self.add(sm.node, e);
        Ok(false)
    }

    fn title_inconsistent(&mut self, source: &str, lineno: isize) -> R<Id> {
        let lb = self.doc.text_elem(Tag::LiteralBlock, "", source);
        self.severe("Title level inconsistent:", MsgArgs::line(lineno).child(lb))
    }

    /// `RSTState.section`.
    fn section(
        &mut self,
        sm: &mut Sm,
        title: &str,
        source: &str,
        style: &str,
        lineno: isize,
        messages: Vec<Id>,
    ) -> R<()> {
        if self.check_subsection(sm, source, style, lineno)? {
            self.new_subsection(sm, title, lineno, messages)?;
        }
        Ok(())
    }

    /// `RSTState.new_subsection`.
    fn new_subsection(
        &mut self,
        sm: &mut Sm,
        title: &str,
        lineno: isize,
        messages: Vec<Id>,
    ) -> R<()> {
        let mylevel = self.memo.section_level;
        self.memo.section_level += 1;
        let section = self.doc.elem(Tag::Section, "");
        self.add(sm.node, section);
        let (textnodes, title_messages) = self.inline_text(sm, title, lineno)?;
        let titlenode = self.doc.elem(Tag::Title, title);
        self.doc.extend(titlenode, &textnodes);
        let name = fully_normalize_name(&self.doc.astext(titlenode));
        self.doc.push_list(section, "names", &name);
        self.add(section, titlenode);
        self.doc.extend(section, &messages);
        self.doc.extend(section, &title_messages);
        self.note_implicit_target(section, Some(section))?;
        let offset = (sm.line_offset + 1) as usize;
        let absoffset = sm.abs_line_offset() + 1;
        let block = sm.lines.slice_from(offset);
        let newabsoffset = self.nested_parse(block, absoffset, section, true)?;
        self.goto_line(sm, newabsoffset);
        if self.memo.section_level <= mylevel {
            return Err(Flow::Eof);
        }
        self.memo.section_level = mylevel;
        Ok(())
    }

    /// `RSTState.paragraph`.
    fn paragraph(&mut self, sm: &mut Sm, lines: &[String], lineno: isize) -> R<(Vec<Id>, bool)> {
        let data = rstrip(&lines.join("\n")).to_string();
        let dchars: Vec<char> = data.chars().collect();
        let (text, literalnext) = if self.pats.literal_marker.search_str(&data).is_some() {
            if dchars.len() == 2 {
                return Ok((Vec::new(), true));
            } else if matches!(dchars[dchars.len() - 3], ' ' | '\n') {
                (
                    rstrip(&dchars[..dchars.len() - 3].iter().collect::<String>()).to_string(),
                    true,
                )
            } else {
                (dchars[..dchars.len() - 1].iter().collect::<String>(), true)
            }
        } else {
            (data.clone(), false)
        };
        let (textnodes, messages) = self.inline_text(sm, &text, lineno)?;
        let p = self.doc.elem(Tag::Paragraph, &data);
        self.doc.extend(p, &textnodes);
        let (s, l) = sm.get_source_and_line(Some(lineno));
        self.doc.nm(p).source = s;
        self.doc.nm(p).line = l;
        let mut out = vec![p];
        out.extend(messages);
        Ok((out, literalnext))
    }

    /// `RSTState.inline_text`.
    pub(super) fn inline_text(
        &mut self,
        sm: &Sm,
        text: &str,
        lineno: isize,
    ) -> R<(Vec<Id>, Vec<Id>)> {
        let parent = sm.node;
        self.inline_parse(text, lineno, parent)
    }

    /// `document.note_implicit_target`.
    pub(super) fn note_implicit_target(&mut self, target: Id, msgnode: Option<Id>) -> R<()> {
        let id = self.doc.set_id(target);
        self.set_name_id_map(target, &id, msgnode, false)
    }

    /// `document.note_explicit_target`.
    pub(super) fn note_explicit_target(&mut self, target: Id, msgnode: Option<Id>) -> R<()> {
        let id = self.doc.set_id(target);
        self.set_name_id_map(target, &id, msgnode, true)
    }

    fn set_name_id_map(
        &mut self,
        node: Id,
        id: &str,
        msgnode: Option<Id>,
        explicit: bool,
    ) -> R<()> {
        for name in self.doc.list(node, "names") {
            if self.doc.nameids.contains_key(&name) {
                self.set_duplicate_name_id(node, id, &name, msgnode, explicit)?;
            } else {
                self.doc.nameids.insert(name.clone(), Some(id.to_string()));
                self.doc.nametypes.insert(name, explicit);
            }
        }
        Ok(())
    }

    fn set_duplicate_name_id(
        &mut self,
        node: Id,
        id: &str,
        name: &str,
        msgnode: Option<Id>,
        explicit: bool,
    ) -> R<()> {
        let old_id = self.doc.nameids[name].clone();
        let old_explicit = self.doc.nametypes[name];
        self.doc
            .nametypes
            .insert(name.to_string(), old_explicit || explicit);
        if explicit {
            if old_explicit {
                let mut level = 2;
                if let Some(oid) = &old_id {
                    let old_node = self.doc.ids[oid];
                    if let Some(refuri) = self.doc.get_str(node, "refuri")
                        && !self.doc.list(old_node, "names").is_empty()
                        && self.doc.get_str(old_node, "refuri").as_deref() == Some(refuri.as_str())
                    {
                        level = 1;
                    }
                    if level > 1 {
                        self.doc.dupname(old_node, name);
                        self.doc.nameids.insert(name.to_string(), None);
                    }
                }
                let msg = self.system_message(
                    level,
                    &format!("Duplicate explicit target name: \"{name}\"."),
                    MsgArgs {
                        backrefs: vec![id.to_string()],
                        base_node: Some(node),
                        ..Default::default()
                    },
                )?;
                if let Some(m) = msgnode {
                    self.add(m, msg);
                }
                self.doc.dupname(node, name);
            } else {
                self.doc
                    .nameids
                    .insert(name.to_string(), Some(id.to_string()));
                if let Some(oid) = &old_id {
                    let old_node = self.doc.ids[oid];
                    self.doc.dupname(old_node, name);
                }
            }
        } else {
            if let Some(oid) = &old_id
                && !old_explicit
            {
                self.doc.nameids.insert(name.to_string(), None);
                let old_node = self.doc.ids[oid];
                self.doc.dupname(old_node, name);
            }
            self.doc.dupname(node, name);
        }
        if !explicit || (!old_explicit && old_id.is_some()) {
            let msg = self.info(
                &format!("Duplicate implicit target name: \"{name}\"."),
                MsgArgs {
                    backrefs: vec![id.to_string()],
                    base_node: Some(node),
                    ..Default::default()
                },
            )?;
            if let Some(m) = msgnode {
                self.add(m, msg);
            }
        }
        Ok(())
    }

    // ----- Body -----

    /// `Body.indent`.
    fn body_indent(&mut self, sm: &mut Sm, next: St) -> R<St> {
        let (indented, _, line_offset, bf) = self.get_indented(sm, false, true)?;
        let elements = self.block_quote(sm, indented, line_offset)?;
        self.doc.extend(sm.node, &elements);
        if !bf {
            let w = self.unindent_warning(sm, "Block quote")?;
            self.add(sm.node, w);
        }
        Ok(next)
    }

    /// `Body.block_quote`.
    fn block_quote(
        &mut self,
        sm: &mut Sm,
        mut indented: StringList,
        mut line_offset: isize,
    ) -> R<Vec<Id>> {
        let mut elements = Vec::new();
        while !indented.is_empty() {
            let bq = self.doc.elem(Tag::BlockQuote, &indented.data.join("\n"));
            let (s, l) = sm.get_source_and_line(Some(line_offset + 1));
            self.doc.nm(bq).source = s;
            self.doc.nm(bq).line = l;
            let (bq_lines, attribution, attribution_offset, rest, new_line_offset) =
                self.split_attribution(&indented, line_offset);
            self.nested_parse(bq_lines, line_offset, bq, false)?;
            elements.push(bq);
            if let Some(a_lines) = attribution {
                let (attr, messages) = self.parse_attribution(
                    sm,
                    &a_lines,
                    line_offset + attribution_offset as isize,
                )?;
                self.add(bq, attr);
                elements.extend(messages);
            }
            line_offset = new_line_offset;
            indented = rest;
            while !indented.is_empty() && indented.data[0].is_empty() {
                indented = indented.slice_from(1);
                line_offset += 1;
            }
        }
        Ok(elements)
    }

    /// `Body.split_attribution`.
    fn split_attribution(
        &self,
        indented: &StringList,
        line_offset: isize,
    ) -> (StringList, Option<StringList>, usize, StringList, isize) {
        let mut blank: Option<usize> = None;
        let mut nonblank_seen = false;
        for i in 0..indented.len() {
            let line = rstrip(&indented.data[i]);
            if !line.is_empty() {
                if nonblank_seen
                    && blank == i.checked_sub(1)
                    && let Some(m) = self.pats.attribution.match_str(line)
                {
                    let (end, indent) = check_attribution(indented, i);
                    if let Some(end) = end {
                        let mut a_lines = indented.slice(i, end);
                        a_lines.trim_left(m.end(), 0, 1);
                        let l = a_lines.len();
                        a_lines.trim_left(indent, 1, l);
                        return (
                            indented.slice(0, i),
                            Some(a_lines),
                            i,
                            indented.slice_from(end),
                            line_offset + end as isize,
                        );
                    }
                }
                nonblank_seen = true;
            } else {
                blank = Some(i);
            }
        }
        (indented.clone(), None, 0, StringList::default(), 0)
    }

    /// `Body.parse_attribution`.
    fn parse_attribution(
        &mut self,
        sm: &mut Sm,
        indented: &StringList,
        line_offset: isize,
    ) -> R<(Id, Vec<Id>)> {
        let text = rstrip(&indented.data.join("\n")).to_string();
        let lineno = 1 + line_offset;
        let (textnodes, messages) = self.inline_text(sm, &text, lineno)?;
        let node = self.doc.elem(Tag::Attribution, &text);
        self.doc.extend(node, &textnodes);
        let (s, l) = sm.get_source_and_line(Some(lineno));
        self.doc.nm(node).source = s;
        self.doc.nm(node).line = l;
        Ok((node, messages))
    }

    /// `Body.bullet`.
    fn bullet(&mut self, sm: &mut Sm, m: &Match, next: St) -> R<St> {
        let bl = self.doc.elem(Tag::BulletList, "");
        let (s, l) = sm.get_source_and_line(None);
        self.doc.nm(bl).source = s;
        self.doc.nm(bl).line = l;
        self.add(sm.node, bl);
        let first: String = sm
            .cur()
            .chars()
            .next()
            .map(String::from)
            .unwrap_or_default();
        self.doc.set_str(bl, "bullet", &first);
        let (item, bf) = self.list_item(sm, m.end())?;
        self.add(bl, item);
        let rest = Self::rest(sm);
        let (new_off, bf) = self.nested_list_parse(
            rest,
            sm.abs_line_offset() + 1,
            bl,
            St::BulletList,
            bf,
            None,
            false,
            &|_| {},
        )?;
        self.goto_line(sm, new_off);
        if !bf {
            let w = self.unindent_warning(sm, "Bullet list")?;
            self.add(sm.node, w);
        }
        Ok(next)
    }

    /// `Body.list_item`.
    fn list_item(&mut self, sm: &mut Sm, indent: usize) -> R<(Id, bool)> {
        let (indented, line_offset, bf) = if !slice_from(&sm.cur(), indent).is_empty() {
            self.get_known_indented(sm, indent)?
        } else {
            let (i, _, o, b) = self.get_first_known_indented(sm, indent, false, true, true)?;
            (i, o, b)
        };
        let item = self.doc.elem(Tag::ListItem, &indented.data.join("\n"));
        if !indented.is_empty() {
            self.nested_parse(indented, line_offset, item, false)?;
        }
        Ok((item, bf))
    }

    /// `Body.enumerator`.
    fn enumerator(&mut self, sm: &mut Sm, m: &Match, next: St) -> R<St> {
        let (format, sequence, text, ordinal) = self.parse_enumerator(m, None);
        if !self.is_enumerated_list_item(sm, ordinal, &sequence, &format)? {
            return Err(Flow::TransCorr("text"));
        }
        let el = self.doc.elem(Tag::EnumeratedList, "");
        self.add(sm.node, el);
        let enumtype = if sequence == "#" {
            "arabic"
        } else {
            sequence.as_str()
        };
        self.doc.set_str(el, "enumtype", enumtype);
        let (prefix, suffix) = format_affixes(&format);
        self.doc.set_str(el, "prefix", prefix);
        self.doc.set_str(el, "suffix", suffix);
        let ordinal = ordinal.unwrap();
        if ordinal != 1 {
            self.doc.set(el, "start", Val::Int(ordinal));
            let msg = self.info(
                &format!(
                    "Enumerated list start value not ordinal-1: \"{text}\" (ordinal {ordinal})"
                ),
                MsgArgs::default(),
            )?;
            self.add(sm.node, msg);
        }
        let (item, bf) = self.list_item(sm, m.end())?;
        self.add(el, item);
        let rest = Self::rest(sm);
        let auto = sequence == "#";
        let fmt = format.clone();
        let (new_off, bf) = self.nested_list_parse(
            rest,
            sm.abs_line_offset() + 1,
            el,
            St::EnumeratedList,
            bf,
            None,
            false,
            &move |s: &mut Sm| {
                s.lastordinal = ordinal;
                s.format = fmt.clone();
                s.auto = auto;
            },
        )?;
        self.goto_line(sm, new_off);
        if !bf {
            let w = self.unindent_warning(sm, "Enumerated list")?;
            self.add(sm.node, w);
        }
        Ok(next)
    }

    /// `EnumeratedList.enumerator`.
    fn enumerated_list_item(&mut self, sm: &mut Sm, m: &Match, next: St) -> R<St> {
        let enumtype = self.doc.get_str(sm.node, "enumtype").unwrap_or_default();
        let (format, sequence, _, ordinal) = self.parse_enumerator(m, Some(&enumtype));
        if format != sm.format
            || (sequence != "#"
                && (sequence != enumtype || sm.auto || ordinal != Some(sm.lastordinal + 1)))
            || !self.is_enumerated_list_item(sm, ordinal, &sequence, &format)?
        {
            return self.invalid_input(sm);
        }
        if sequence == "#" {
            sm.auto = true;
        }
        let (item, bf) = self.list_item(sm, m.end())?;
        let node = sm.node;
        self.add(node, item);
        sm.set_bf(St::EnumeratedList, bf);
        sm.lastordinal = ordinal.unwrap_or(0);
        Ok(next)
    }

    /// `Body.parse_enumerator`.
    fn parse_enumerator(
        &self,
        m: &Match,
        expected: Option<&str>,
    ) -> (String, String, String, Option<i64>) {
        let mut format = "";
        for f in ["parens", "rparen", "period"] {
            if m.has(f) {
                format = f;
                break;
            }
        }
        let g = m.name(format).unwrap();
        let gc: Vec<char> = g.chars().collect();
        let text: String = match format {
            "parens" => gc[1..gc.len() - 1].iter().collect(),
            _ => gc[..gc.len() - 1].iter().collect(),
        };
        let mut sequence = String::new();
        if text == "#" {
            sequence = "#".into();
        } else if let Some(exp) = expected {
            if let Some((_, re)) = self.pats.sequences.iter().find(|(n, _)| *n == exp)
                && re.is_match(&text)
            {
                sequence = exp.to_string();
            }
        } else if text == "i" {
            sequence = "lowerroman".into();
        } else if text == "I" {
            sequence = "upperroman".into();
        }
        if sequence.is_empty() {
            for (name, re) in &self.pats.sequences {
                if re.is_match(&text) {
                    sequence = name.to_string();
                    break;
                }
            }
        }
        let ordinal = if sequence == "#" {
            Some(1)
        } else {
            match sequence.as_str() {
                "arabic" => text.parse::<i64>().ok(),
                "loweralpha" => Some(text.chars().next().unwrap() as i64 - 'a' as i64 + 1),
                "upperalpha" => Some(text.chars().next().unwrap() as i64 - 'A' as i64 + 1),
                "lowerroman" => from_roman(&text.to_uppercase()),
                _ => from_roman(&text),
            }
        };
        (format.to_string(), sequence, text, ordinal)
    }

    /// `Body.is_enumerated_list_item`.
    fn is_enumerated_list_item(
        &mut self,
        sm: &mut Sm,
        ordinal: Option<i64>,
        sequence: &str,
        format: &str,
    ) -> R<bool> {
        let Some(ordinal) = ordinal else {
            return Ok(false);
        };
        let next_line = match self.next_line(sm, 1) {
            Ok(l) => {
                self.previous_line(sm, 1);
                l
            }
            Err(Flow::Eof) => {
                self.previous_line(sm, 1);
                return Ok(true);
            }
            Err(e) => return Err(e),
        };
        if strip(slice_to(&next_line, 1)).is_empty() {
            return Ok(true);
        }
        if let Some((next_enum, auto_enum)) = make_enumerator(ordinal + 1, sequence, format)
            && (next_line.starts_with(&next_enum) || next_line.starts_with(&auto_enum))
        {
            return Ok(true);
        }
        Ok(false)
    }

    /// `Body.field_marker`.
    fn field_marker(&mut self, sm: &mut Sm, m: &Match, next: St) -> R<St> {
        let fl = self.doc.elem(Tag::FieldList, "");
        self.add(sm.node, fl);
        let (field, bf) = self.field(sm, m, false)?;
        self.add(fl, field);
        let rest = Self::rest(sm);
        let (new_off, bf) = self.nested_list_parse(
            rest,
            sm.abs_line_offset() + 1,
            fl,
            St::FieldList,
            bf,
            None,
            false,
            &|_| {},
        )?;
        self.goto_line(sm, new_off);
        if !bf {
            let w = self.unindent_warning(sm, "Field list")?;
            self.add(sm.node, w);
        }
        Ok(next)
    }

    /// `Body.field` (`ExtensionOptions.parse_field_body` when `simple`).
    fn field(&mut self, sm: &mut Sm, m: &Match, simple: bool) -> R<(Id, bool)> {
        let name = parse_field_marker(m);
        let (src, srcline) = sm.get_source_and_line(None);
        let lineno = sm.abs_line_number();
        let (indented, _, line_offset, bf) =
            self.get_first_known_indented(sm, m.end(), false, true, true)?;
        let field = self.doc.elem(Tag::Field, "");
        self.doc.nm(field).source = src;
        self.doc.nm(field).line = srcline;
        let (name_nodes, name_messages) = self.inline_text(sm, &name, lineno)?;
        let fname = self.doc.elem(Tag::FieldName, &name);
        self.doc.extend(fname, &name_nodes);
        self.add(field, fname);
        let body = self.doc.elem(Tag::FieldBody, &indented.data.join("\n"));
        self.doc.extend(body, &name_messages);
        self.add(field, body);
        if !indented.is_empty() {
            if simple {
                let mut lines: Vec<String> = Vec::new();
                let mut all = indented.data.clone();
                all.push(String::new());
                for line in all {
                    if !strip(&line).is_empty() {
                        lines.push(line);
                    } else if !lines.is_empty() {
                        let text = lines.join("\n");
                        let p = self.doc.text_elem(Tag::Paragraph, &text, &text);
                        self.add(body, p);
                        lines.clear();
                    }
                }
            } else {
                self.nested_parse(indented, line_offset, body, false)?;
            }
        }
        Ok((field, bf))
    }

    /// `Body.option_marker`.
    fn option_marker(&mut self, sm: &mut Sm, m: &Match, next: St) -> R<St> {
        let ol = self.doc.elem(Tag::OptionList, "");
        let (s, l) = sm.get_source_and_line(None);
        self.doc.nm(ol).source = s;
        self.doc.nm(ol).line = l;
        let (item, bf) = match self.option_list_item(sm, m) {
            Ok(r) => r,
            Err(Flow::Markup(detail)) => {
                let msg = self.error(
                    &format!("Invalid option list marker: {detail}"),
                    MsgArgs::default(),
                )?;
                self.add(sm.node, msg);
                let (indented, _, line_offset, bf) =
                    self.get_first_known_indented(sm, m.end(), false, true, true)?;
                let elements = self.block_quote(sm, indented, line_offset)?;
                self.doc.extend(sm.node, &elements);
                if !bf {
                    let w = self.unindent_warning(sm, "Option list")?;
                    self.add(sm.node, w);
                }
                return Ok(next);
            }
            Err(e) => return Err(e),
        };
        self.add(sm.node, ol);
        self.add(ol, item);
        let rest = Self::rest(sm);
        let (new_off, bf) = self.nested_list_parse(
            rest,
            sm.abs_line_offset() + 1,
            ol,
            St::OptionList,
            bf,
            None,
            false,
            &|_| {},
        )?;
        self.goto_line(sm, new_off);
        if !bf {
            let w = self.unindent_warning(sm, "Option list")?;
            self.add(sm.node, w);
        }
        Ok(next)
    }

    /// `Body.option_list_item`.
    fn option_list_item(&mut self, sm: &mut Sm, m: &Match) -> R<(Id, bool)> {
        let offset = sm.abs_line_offset();
        let options = self.parse_option_marker(m)?;
        let (indented, _, line_offset, bf) =
            self.get_first_known_indented(sm, m.end(), false, true, true)?;
        if indented.is_empty() {
            self.goto_line(sm, offset);
            return Err(Flow::TransCorr("text"));
        }
        let group = self.doc.elem(Tag::OptionGroup, "");
        self.doc.extend(group, &options);
        let desc = self.doc.elem(Tag::Description, &indented.data.join("\n"));
        let item = self.doc.elem(Tag::OptionListItem, "");
        self.add(item, group);
        self.add(item, desc);
        self.nested_parse(indented, line_offset, desc, false)?;
        Ok((item, bf))
    }

    /// `Body.parse_option_marker`.
    fn parse_option_marker(&mut self, m: &Match) -> R<Vec<Id>> {
        let mut optlist = Vec::new();
        let whole: String = m.group(0).unwrap();
        for optionstring in rstrip(&whole).split(", ") {
            let mut tokens: Vec<String> = split_ws(optionstring)
                .iter()
                .map(|s| s.to_string())
                .collect();
            let mut delimiter = " ";
            let first = tokens[0].clone();
            if let Some((a, b)) = first.split_once('=') {
                tokens.splice(0..1, [a.to_string(), b.to_string()]);
                delimiter = "=";
            } else if clen(&first) > 2
                && ((first.starts_with('-') && !first.starts_with("--")) || first.starts_with('+'))
            {
                tokens.splice(
                    0..1,
                    [
                        slice(&first, 0, 2).to_string(),
                        slice_from(&first, 2).to_string(),
                    ],
                );
                delimiter = "";
            }
            if tokens.len() > 1
                && tokens[1].starts_with('<')
                && tokens.last().unwrap().ends_with('>')
            {
                let joined = tokens[1..].join(" ");
                tokens.truncate(1);
                tokens.push(joined);
            }
            if !tokens.is_empty() && tokens.len() <= 2 {
                let option = self.doc.elem(Tag::Option, optionstring);
                let os = self
                    .doc
                    .text_elem(Tag::OptionString, &tokens[0], &tokens[0]);
                self.add(option, os);
                if tokens.len() > 1 {
                    let oa = self
                        .doc
                        .text_elem(Tag::OptionArgument, &tokens[1], &tokens[1]);
                    self.doc.set_str(oa, "delimiter", delimiter);
                    self.add(option, oa);
                }
                optlist.push(option);
            } else {
                return Err(Flow::Markup(format!(
                    "wrong number of option tokens (={}), should be 1 or 2: \"{}\"",
                    tokens.len(),
                    optionstring
                )));
            }
        }
        Ok(optlist)
    }

    /// `Body.line_block`.
    fn line_block(&mut self, sm: &mut Sm, m: &Match, next: St) -> R<St> {
        let block = self.doc.elem(Tag::LineBlock, "");
        self.add(sm.node, block);
        let lineno = sm.abs_line_number();
        let (line, messages, mut bf) = self.line_block_line(sm, m, lineno)?;
        self.add(block, line);
        self.doc.extend(sm.node, &messages);
        if !bf {
            let rest = Self::rest(sm);
            let (new_off, b) = self.nested_list_parse(
                rest,
                sm.abs_line_offset() + 1,
                block,
                St::LineBlock,
                false,
                None,
                false,
                &|_| {},
            )?;
            self.goto_line(sm, new_off);
            bf = b;
        }
        if !bf {
            let w = self.warning(
                "Line block ends without a blank line.",
                MsgArgs::line(lineno + 1),
            )?;
            self.add(sm.node, w);
        }
        if !self.doc.children(block).is_empty() {
            let first = self.doc.children(block)[0];
            if self.doc.n(first).indent.is_none() {
                self.doc.nm(first).indent = Some(0);
            }
            self.nest_line_block_lines(block);
        }
        Ok(next)
    }

    /// `Body.line_block_line`.
    fn line_block_line(&mut self, sm: &mut Sm, m: &Match, lineno: isize) -> R<(Id, Vec<Id>, bool)> {
        let (indented, _, _, bf) = self.get_first_known_indented(sm, m.end(), true, true, true)?;
        let text = indented.data.join("\n");
        let (text_nodes, messages) = self.inline_text(sm, &text, lineno)?;
        let line = self.doc.elem(Tag::Line, &text);
        self.doc.extend(line, &text_nodes);
        let s: String = m.string().iter().collect();
        if rstrip(&s) != "|" {
            let g1 = m.group(1).unwrap();
            self.doc.nm(line).indent = Some(clen(&g1) - 1);
        }
        Ok((line, messages, bf))
    }

    fn nest_line_block_lines(&mut self, block: Id) {
        let ch = self.doc.children(block).to_vec();
        for i in 1..ch.len() {
            if self.doc.n(ch[i]).indent.is_none() {
                self.doc.nm(ch[i]).indent = self.doc.n(ch[i - 1]).indent;
            }
        }
        self.nest_line_block_segment(block);
    }

    fn nest_line_block_segment(&mut self, block: Id) {
        let items = self.doc.children(block).to_vec();
        let least = items
            .iter()
            .map(|&i| self.doc.n(i).indent.unwrap_or(0))
            .min()
            .unwrap_or(0);
        let mut new_items = Vec::new();
        let mut new_block = self.doc.elem(Tag::LineBlock, "");
        for item in items {
            if self.doc.n(item).indent.unwrap_or(0) > least {
                self.doc.append(new_block, item);
            } else {
                if !self.doc.children(new_block).is_empty() {
                    self.nest_line_block_segment(new_block);
                    new_items.push(new_block);
                    new_block = self.doc.elem(Tag::LineBlock, "");
                }
                new_items.push(item);
            }
        }
        if !self.doc.children(new_block).is_empty() {
            self.nest_line_block_segment(new_block);
            new_items.push(new_block);
        }
        self.doc.set_children(block, new_items);
    }

    /// `Body.table_top`.
    fn table_top(&mut self, sm: &mut Sm, next: St, grid: bool) -> R<St> {
        let (nodelist, bf) = self.table(sm, grid)?;
        self.doc.extend(sm.node, &nodelist);
        if !bf {
            let msg = self.warning(
                "Blank line required after table.",
                MsgArgs::line(sm.abs_line_number() + 1),
            )?;
            self.add(sm.node, msg);
        }
        Ok(next)
    }

    /// `Body.line`.
    fn body_line(&mut self, sm: &mut Sm, _m: &Match, next: St, context: &mut Vec<String>) -> R<St> {
        let line = sm.cur();
        if sm.match_titles {
            *context = vec![line];
            return Ok(St::Line);
        }
        if strip(&line) == "::" {
            return Err(Flow::TransCorr("text"));
        }
        if clen(strip(&line)) < 4 {
            let msg = self.info(
                "Unexpected possible title overline or transition.\nTreating it as ordinary text because it's so short.",
                MsgArgs::line(sm.abs_line_number()),
            )?;
            self.add(sm.node, msg);
            return Err(Flow::TransCorr("text"));
        }
        let lb = self.literal(&line);
        let msg = self.severe(
            "Unexpected section title or transition.",
            MsgArgs::line(sm.abs_line_number()).child(lb),
        )?;
        self.add(sm.node, msg);
        Ok(next)
    }

    // ----- Text, Definition, Line -----

    fn text_dispatch(
        &mut self,
        sm: &mut Sm,
        st: St,
        name: &str,
        m: Match,
        context: &mut Vec<String>,
    ) -> R<St> {
        match (st, name) {
            (St::Text, "blank") => {
                self.text_blank(sm, context)?;
                *context = Vec::new();
                Ok(St::Body)
            }
            (St::Text, "indent") => self.text_indent(sm, context),
            (St::Text, "underline") => self.text_underline(sm, &m, context),
            (St::Text, "text") => self.text_text(sm, context),
            (St::Definition, "indent") => {
                let (item, bf) = self.definition_list_item(sm, context)?;
                let node = sm.node;
                self.add(node, item);
                sm.set_bf(St::Definition, bf);
                *context = Vec::new();
                Ok(St::DefinitionList)
            }
            (St::Definition, _) => Err(Flow::Eof),
            (St::Line, "blank") => {
                let (src, srcline) = sm.get_source_and_line(None);
                let marker = strip(&context[0]).to_string();
                if clen(&marker) < 4 {
                    self.state_correction(sm, context, 1)?;
                }
                let t = self.doc.elem(Tag::Transition, &marker);
                self.doc.nm(t).source = src;
                self.doc.nm(t).line = srcline.map(|l| l - 1);
                self.add(sm.node, t);
                *context = Vec::new();
                Ok(St::Body)
            }
            (St::Line, "text" | "indent") => self.line_text(sm, &m, context),
            (St::Line, "underline") => {
                let overline = context[0].clone();
                let blocktext = format!("{}\n{}", overline, sm.cur());
                let lineno = sm.abs_line_number() - 1;
                if clen(rstrip(&overline)) < 4 {
                    self.short_overline(sm, context, lineno, 1)?;
                }
                let lb = self.literal(&blocktext);
                let msg = self.error(
                    "Invalid section title or transition marker.",
                    MsgArgs::line(lineno).child(lb),
                )?;
                self.add(sm.node, msg);
                *context = Vec::new();
                Ok(St::Body)
            }
            _ => unreachable!(),
        }
    }

    /// `Text.blank`.
    fn text_blank(&mut self, sm: &mut Sm, context: &[String]) -> R<()> {
        let (paragraph, literalnext) = self.paragraph(sm, context, sm.abs_line_number() - 1)?;
        self.doc.extend(sm.node, &paragraph);
        if literalnext {
            let lb = self.literal_block(sm)?;
            self.doc.extend(sm.node, &lb);
        }
        Ok(())
    }

    /// `Text.indent`.
    fn text_indent(&mut self, sm: &mut Sm, context: &mut Vec<String>) -> R<St> {
        let dl = self.doc.elem(Tag::DefinitionList, "");
        let (item, bf) = self.definition_list_item(sm, context)?;
        self.add(dl, item);
        self.add(sm.node, dl);
        let rest = Self::rest(sm);
        let (new_off, bf) = self.nested_list_parse(
            rest,
            sm.abs_line_offset() + 1,
            dl,
            St::DefinitionList,
            bf,
            Some(St::Definition),
            false,
            &|_| {},
        )?;
        self.goto_line(sm, new_off);
        if !bf {
            let w = self.unindent_warning(sm, "Definition list")?;
            self.add(sm.node, w);
        }
        *context = Vec::new();
        Ok(St::Body)
    }

    /// `Text.underline`.
    fn text_underline(&mut self, sm: &mut Sm, _m: &Match, context: &mut Vec<String>) -> R<St> {
        let lineno = sm.abs_line_number();
        let title = rstrip(&context[0]).to_string();
        let underline = rstrip(&sm.cur()).to_string();
        let source = format!("{title}\n{underline}");
        let mut messages = Vec::new();
        if column_width(&title) > clen(&underline) {
            if clen(&underline) < 4 {
                if sm.match_titles {
                    let msg = self.info(
                        "Possible title underline, too short for the title.\nTreating it as ordinary text because it's so short.",
                        MsgArgs::line(lineno),
                    )?;
                    self.add(sm.node, msg);
                }
                return Err(Flow::TransCorr("text"));
            }
            let blocktext = format!("{}\n{}", context[0], sm.cur());
            let lb = self.literal(&blocktext);
            let msg = self.warning(
                "Title underline too short.",
                MsgArgs::line(lineno).child(lb),
            )?;
            messages.push(msg);
        }
        if !sm.match_titles {
            let blocktext = format!("{}\n{}", context[0], sm.cur());
            let (src, srcline) = sm.get_source_and_line(None);
            let lb = self.literal(&blocktext);
            let msg = self.severe(
                "Unexpected section title.",
                MsgArgs {
                    source: src,
                    line: srcline.map(|l| l as isize),
                    children: vec![lb],
                    ..Default::default()
                },
            )?;
            self.doc.extend(sm.node, &messages);
            self.add(sm.node, msg);
            *context = Vec::new();
            return Ok(St::Body);
        }
        let style: String = underline.chars().next().map(String::from).unwrap();
        context.clear();
        self.section(sm, &title, &source, &style, lineno - 1, messages)?;
        Ok(St::Body)
    }

    /// `Text.text`.
    fn text_text(&mut self, sm: &mut Sm, context: &mut Vec<String>) -> R<St> {
        let startline = sm.abs_line_number() - 1;
        let mut msg = None;
        let block = match self.get_text_block(sm, true)? {
            Ok(b) => b,
            Err((b, srcline)) => {
                msg = Some(self.error(
                    "Unexpected indentation.",
                    MsgArgs {
                        source: Some("<string>".into()),
                        line: srcline.map(|l| l as isize),
                        ..Default::default()
                    },
                )?);
                b
            }
        };
        let mut lines = context.clone();
        lines.extend(block.data);
        let (paragraph, literalnext) = self.paragraph(sm, &lines, startline)?;
        self.doc.extend(sm.node, &paragraph);
        if let Some(m) = msg {
            self.add(sm.node, m);
        }
        if literalnext {
            match self.next_line(sm, 1) {
                Ok(_) | Err(Flow::Eof) => {}
                Err(e) => return Err(e),
            }
            let lb = self.literal_block(sm)?;
            self.doc.extend(sm.node, &lb);
        }
        *context = Vec::new();
        Ok(St::Body)
    }

    /// `Text.literal_block`.
    fn literal_block(&mut self, sm: &mut Sm) -> R<Vec<Id>> {
        let (mut indented, _, offset, bf) = self.get_indented(sm, false, true)?;
        while !indented.is_empty() && strip(indented.data.last().unwrap()).is_empty() {
            indented.trim_end(1);
        }
        if indented.is_empty() {
            return self.quoted_literal_block(sm);
        }
        let data = indented.data.join("\n");
        let lb = self.literal(&data);
        let (s, l) = sm.get_source_and_line(Some(offset + 1));
        self.doc.nm(lb).source = s;
        self.doc.nm(lb).line = l;
        let mut out = vec![lb];
        if !bf {
            out.push(self.unindent_warning(sm, "Literal block")?);
        }
        Ok(out)
    }

    /// `Text.quoted_literal_block`.
    fn quoted_literal_block(&mut self, sm: &mut Sm) -> R<Vec<Id>> {
        let abs = sm.abs_line_offset();
        let offset = sm.line_offset.max(0) as usize;
        let parent = self.doc.elem(Tag::Element, "");
        let block = sm.lines.slice_from(offset);
        let mut nsm = Sm::new(block, abs as usize, parent, false);
        self.run(&mut nsm, St::QuotedLiteralBlock)?;
        let new_abs = nsm.abs_line_offset();
        self.goto_line(sm, new_abs);
        Ok(self.doc.children(parent).to_vec())
    }

    /// `Text.definition_list_item`.
    fn definition_list_item(&mut self, sm: &mut Sm, termline: &[String]) -> R<(Id, bool)> {
        let (indented, _, line_offset, bf) = self.get_indented(sm, false, true)?;
        let mut raw = termline.to_vec();
        raw.extend(indented.data.iter().cloned());
        let item = self.doc.elem(Tag::DefinitionListItem, &raw.join("\n"));
        let lineno = sm.abs_line_number() - 1;
        let (s, l) = sm.get_source_and_line(Some(lineno));
        self.doc.nm(item).source = s;
        self.doc.nm(item).line = l;
        let (termlist, messages) = self.term(sm, termline, lineno)?;
        self.doc.extend(item, &termlist);
        let definition = self.doc.elem(Tag::Definition, "");
        self.doc.extend(definition, &messages);
        self.add(item, definition);
        if termline[0].ends_with("::") {
            let msg = self.info(
                "Blank line missing before literal block (after the \"::\")? Interpreted as a definition list item.",
                MsgArgs::line(lineno + 1),
            )?;
            self.add(definition, msg);
        }
        self.nested_parse(indented, line_offset, definition, false)?;
        Ok((item, bf))
    }

    /// `Text.term`.
    fn term(&mut self, sm: &mut Sm, lines: &[String], lineno: isize) -> R<(Vec<Id>, Vec<Id>)> {
        let (text_nodes, messages) = self.inline_text(sm, &lines[0], lineno)?;
        let term = self.doc.elem(Tag::Term, &lines[0]);
        let (s, l) = sm.get_source_and_line(Some(lineno));
        self.doc.nm(term).source = s;
        self.doc.nm(term).line = l;
        let mut node_list = vec![term];
        for node in text_nodes {
            if self.doc.tag(node) == Tag::Text {
                let text = self.doc.n(node).text.clone();
                let parts = split_classifier(&text);
                let last = *node_list.last().unwrap();
                if parts.len() == 1 {
                    self.add(last, node);
                } else {
                    let t = self.doc.text(rstrip(&parts[0]));
                    self.add(last, t);
                    for part in &parts[1..] {
                        let c = self
                            .doc
                            .text_elem(Tag::Classifier, &unescape(part, true), part);
                        node_list.push(c);
                    }
                }
            } else {
                let last = *node_list.last().unwrap();
                self.add(last, node);
            }
        }
        Ok((node_list, messages))
    }

    /// `Line.eof`.
    fn line_eof(&mut self, sm: &mut Sm, context: &mut Vec<String>) -> R<()> {
        let marker = strip(&context[0]).to_string();
        if self.memo.section_bubble_up_kludge {
            self.memo.section_bubble_up_kludge = false;
        } else if clen(&marker) < 4 {
            self.state_correction(sm, context, 1)?;
        }
        if sm.eofcheck {
            let (src, srcline) = sm.get_source_and_line(None);
            let t = self.doc.elem(Tag::Transition, &context[0]);
            self.doc.nm(t).source = src;
            self.doc.nm(t).line = srcline.map(|l| l - 1);
            self.add(sm.node, t);
        }
        sm.eofcheck = true;
        Ok(())
    }

    /// `Line.text`.
    fn line_text(&mut self, sm: &mut Sm, _m: &Match, context: &mut Vec<String>) -> R<St> {
        let lineno = sm.abs_line_number() - 1;
        let overline = context[0].clone();
        let title = sm.cur();
        let underline = match self.next_line(sm, 1) {
            Ok(u) => u,
            Err(Flow::Eof) => {
                let blocktext = format!("{overline}\n{title}");
                if clen(rstrip(&overline)) < 4 {
                    self.short_overline(sm, context, lineno, 2)?;
                    unreachable!();
                } else {
                    let lb = self.literal(&blocktext);
                    let msg =
                        self.severe("Incomplete section title.", MsgArgs::line(lineno).child(lb))?;
                    self.add(sm.node, msg);
                    *context = Vec::new();
                    return Ok(St::Body);
                }
            }
            Err(e) => return Err(e),
        };
        let source = format!("{overline}\n{title}\n{underline}");
        let overline = rstrip(&overline).to_string();
        let underline = rstrip(&underline).to_string();
        if !self.pats.line.is_match(&underline) {
            if clen(&overline) < 4 {
                self.short_overline(sm, context, lineno, 2)?;
            } else {
                let lb = self.literal(&source);
                let msg = self.severe(
                    "Missing matching underline for section title overline.",
                    MsgArgs::line(lineno).child(lb),
                )?;
                self.add(sm.node, msg);
                *context = Vec::new();
                return Ok(St::Body);
            }
        } else if overline != underline {
            if clen(&overline) < 4 {
                self.short_overline(sm, context, lineno, 2)?;
            } else {
                let lb = self.literal(&source);
                let msg = self.severe(
                    "Title overline & underline mismatch.",
                    MsgArgs::line(lineno).child(lb),
                )?;
                self.add(sm.node, msg);
                *context = Vec::new();
                return Ok(St::Body);
            }
        }
        let title = rstrip(&title).to_string();
        let mut messages = Vec::new();
        if column_width(&title) > clen(&overline) {
            if clen(&overline) < 4 {
                self.short_overline(sm, context, lineno, 2)?;
            } else {
                let lb = self.literal(&source);
                let msg =
                    self.warning("Title overline too short.", MsgArgs::line(lineno).child(lb))?;
                messages.push(msg);
            }
        }
        let style: String = [
            overline.chars().next().unwrap(),
            underline.chars().next().unwrap(),
        ]
        .iter()
        .collect();
        sm.eofcheck = false;
        self.section(
            sm,
            super::chars::lstrip(&title),
            &source,
            &style,
            lineno + 1,
            messages,
        )?;
        sm.eofcheck = true;
        *context = Vec::new();
        Ok(St::Body)
    }

    /// `Line.short_overline`.
    fn short_overline(
        &mut self,
        sm: &mut Sm,
        context: &mut Vec<String>,
        lineno: isize,
        lines: isize,
    ) -> R<()> {
        let msg = self.info(
            "Possible incomplete section title.\nTreating the overline as ordinary text because it's so short.",
            MsgArgs::line(lineno),
        )?;
        self.add(sm.node, msg);
        self.state_correction(sm, context, lines)
    }

    /// `Line.state_correction`.
    fn state_correction(&mut self, sm: &mut Sm, context: &mut Vec<String>, lines: isize) -> R<()> {
        self.previous_line(sm, lines);
        context.clear();
        Err(Flow::StateCorr(St::Body, Some("text")))
    }

    // ----- QuotedLiteralBlock -----

    fn qlb_dispatch(
        &mut self,
        sm: &mut Sm,
        name: &str,
        m: Match,
        context: &mut Vec<String>,
        next: St,
    ) -> R<St> {
        match name {
            "blank" => {
                if !context.is_empty() {
                    Err(Flow::Eof)
                } else {
                    Ok(next)
                }
            }
            "indent" => {
                let msg = self.error(
                    "Unexpected indentation.",
                    MsgArgs::line(sm.abs_line_number()),
                )?;
                sm.qlb_messages.push(msg);
                self.previous_line(sm, 1);
                Err(Flow::Eof)
            }
            "initial_quoted" => {
                let s: String = m.string().iter().collect();
                sm.qlb_quote = s.chars().next();
                sm.qlb_initial_lineno = Some(sm.abs_line_number());
                *context = vec![s];
                Ok(next)
            }
            "quoted" => {
                context.push(sm.cur());
                Ok(next)
            }
            _ => {
                if !context.is_empty() {
                    let msg = self.error(
                        "Inconsistent literal block quoting.",
                        MsgArgs::line(sm.abs_line_number()),
                    )?;
                    sm.qlb_messages.push(msg);
                    self.previous_line(sm, 1);
                }
                Err(Flow::Eof)
            }
        }
    }

    fn qlb_eof(&mut self, sm: &mut Sm, context: &mut [String]) -> R<()> {
        if !context.is_empty() {
            let (src, srcline) = sm.get_source_and_line(sm.qlb_initial_lineno);
            let text = context.join("\n");
            let lb = self.literal(&text);
            self.doc.nm(lb).source = src;
            self.doc.nm(lb).line = srcline;
            self.add(sm.node, lb);
        } else {
            let msg = self.warning(
                "Literal block expected; none found.",
                MsgArgs::line(sm.abs_line_number()),
            )?;
            self.add(sm.node, msg);
            self.previous_line(sm, 1);
        }
        let msgs = std::mem::take(&mut sm.qlb_messages);
        self.doc.extend(sm.node, &msgs);
        Ok(())
    }

    // ----- Explicit markup -----

    /// `Body.explicit_construct`.
    fn explicit_construct(&mut self, sm: &mut Sm, m: &Match) -> R<(Vec<Id>, bool)> {
        let line: String = m.string().iter().collect();
        let mut errors = Vec::new();
        let p = self.pats;
        let constructs: [(&Regex, u8); 5] = [
            (&p.ex_footnote, 0),
            (&p.ex_citation, 1),
            (&p.ex_hyperlink_target, 2),
            (&p.ex_substitution_def, 3),
            (&p.ex_directive, 4),
        ];
        for (re, kind) in constructs {
            if let Some(em) = re.match_str(&line) {
                let r = match kind {
                    0 => self.footnote(sm, &em),
                    1 => self.citation(sm, &em),
                    2 => self.hyperlink_target(sm, &em),
                    3 => self.substitution_def(sm, &em),
                    _ => self.directive(sm, &em, None),
                };
                match r {
                    Err(Flow::Markup(message)) => {
                        let lineno = sm.abs_line_number();
                        errors.push(self.warning(&message, MsgArgs::line(lineno))?);
                        break;
                    }
                    other => return other,
                }
            }
        }
        let (mut nodes, bf) = self.comment(sm, m)?;
        nodes.extend(errors);
        Ok((nodes, bf))
    }

    /// `Body.explicit_list`.
    fn explicit_list(&mut self, sm: &mut Sm, blank_finish: bool) -> R<()> {
        let rest = Self::rest(sm);
        let (new_off, bf) = self.nested_list_parse(
            rest,
            sm.abs_line_offset() + 1,
            sm.node,
            St::Explicit,
            blank_finish,
            None,
            sm.match_titles,
            &|_| {},
        )?;
        self.goto_line(sm, new_off);
        if !bf {
            let w = self.unindent_warning(sm, "Explicit markup")?;
            self.add(sm.node, w);
        }
        Ok(())
    }

    /// `Body.comment`.
    fn comment(&mut self, sm: &mut Sm, m: &Match) -> R<(Vec<Id>, bool)> {
        if sm.is_next_line_blank() {
            let s: String = m.string()[m.end()..].iter().collect();
            if strip(&s).is_empty() {
                let c = self.doc.elem(Tag::Comment, "");
                return Ok((vec![c], true));
            }
        }
        let (mut indented, _, _, bf) =
            self.get_first_known_indented(sm, m.end(), false, true, true)?;
        while !indented.is_empty() && strip(indented.data.last().unwrap()).is_empty() {
            indented.trim_end(1);
        }
        let text = indented.data.join("\n");
        let c = self.doc.text_elem(Tag::Comment, &text, &text);
        Ok((vec![c], bf))
    }

    /// `Body.footnote`.
    fn footnote(&mut self, sm: &mut Sm, m: &Match) -> R<(Vec<Id>, bool)> {
        let (src, srcline) = sm.get_source_and_line(None);
        let (indented, _, offset, bf) =
            self.get_first_known_indented(sm, m.end(), false, true, true)?;
        let label = m.group(1).unwrap();
        let mut name = fully_normalize_name(&label);
        let footnote = self.doc.elem(Tag::Footnote, &indented.data.join("\n"));
        self.doc.nm(footnote).source = src;
        self.doc.nm(footnote).line = srcline;
        if let Some(rest) = name.strip_prefix('#') {
            name = rest.to_string();
            self.doc.set(footnote, "auto", Val::Int(1));
            if !name.is_empty() {
                self.doc.push_list(footnote, "names", &name);
            }
            self.doc.set_id(footnote);
            self.doc.autofootnotes.push(footnote);
        } else if name == "*" {
            name = String::new();
            self.doc.set_str(footnote, "auto", "*");
            self.doc.set_id(footnote);
            self.doc.symbol_footnotes.push(footnote);
        } else {
            let l = self.doc.text_elem(Tag::Label, "", &label);
            self.add(footnote, l);
            self.doc.push_list(footnote, "names", &name);
            self.doc.set_id(footnote);
            self.doc.footnotes.push(footnote);
        }
        if !name.is_empty() {
            self.note_explicit_target(footnote, Some(footnote))?;
        } else {
            self.doc.set_id(footnote);
        }
        if !indented.is_empty() {
            self.nested_parse(indented, offset, footnote, false)?;
        }
        Ok((vec![footnote], bf))
    }

    /// `Body.citation`.
    fn citation(&mut self, sm: &mut Sm, m: &Match) -> R<(Vec<Id>, bool)> {
        let (src, srcline) = sm.get_source_and_line(None);
        let (indented, _, offset, bf) =
            self.get_first_known_indented(sm, m.end(), false, true, true)?;
        let label = m.group(1).unwrap();
        let name = fully_normalize_name(&label);
        let citation = self.doc.elem(Tag::Citation, &indented.data.join("\n"));
        self.doc.nm(citation).source = src;
        self.doc.nm(citation).line = srcline;
        let l = self.doc.text_elem(Tag::Label, "", &label);
        self.add(citation, l);
        self.doc.push_list(citation, "names", &name);
        self.doc.citations.push(citation);
        self.note_explicit_target(citation, Some(citation))?;
        if !indented.is_empty() {
            self.nested_parse(indented, offset, citation, false)?;
        }
        Ok((vec![citation], bf))
    }

    /// `Body.hyperlink_target`.
    fn hyperlink_target(&mut self, sm: &mut Sm, m: &Match) -> R<(Vec<Id>, bool)> {
        let lineno = sm.abs_line_number();
        let (block, _, _, bf) = self.get_first_known_indented(sm, m.end(), true, false, true)?;
        let head: String = m.string()[..m.end()].iter().collect();
        let blocktext = format!("{}{}", head, block.data.join("\n"));
        let mut block: Vec<String> = block.data.iter().map(|l| escape2null(l)).collect();
        let mut escaped = block.first().cloned().unwrap_or_default();
        let mut blockindex = 0;
        let targetmatch = loop {
            if let Some(tm) = self.pats.ex_target.match_str(&escaped) {
                break tm;
            }
            blockindex += 1;
            match block.get(blockindex) {
                Some(b) => escaped.push_str(b),
                None => return Err(Flow::Markup("malformed hyperlink target.".into())),
            }
        };
        block.drain(..blockindex);
        let elen = clen(&escaped) as isize;
        let b0 = format!("{} ", block[0]);
        let start = targetmatch.end() as isize - elen - 1;
        let b0len = clen(&b0) as isize;
        let start = if start < 0 {
            (b0len + start).max(0)
        } else {
            start
        } as usize;
        block[0] = strip(slice_from(&b0, start)).to_string();
        let name = targetmatch.name("name");
        let target = self.make_target(
            sm,
            &block,
            &blocktext,
            lineno,
            name.as_deref().unwrap_or(""),
        )?;
        Ok((vec![target], bf))
    }

    /// `Body.make_target`.
    fn make_target(
        &mut self,
        sm: &mut Sm,
        block: &[String],
        block_text: &str,
        lineno: isize,
        target_name: &str,
    ) -> R<Id> {
        let (kind, data) = self.parse_target(block);
        if kind == "refname" {
            let target = self.doc.elem(Tag::Target, block_text);
            self.doc
                .set_str(target, "refname", &fully_normalize_name(&data));
            self.add_target(sm, target_name, "", target, lineno)?;
            self.doc.note_indirect_target(target);
            Ok(target)
        } else {
            let target = self.doc.elem(Tag::Target, block_text);
            self.add_target(sm, target_name, &data, target, lineno)?;
            Ok(target)
        }
    }

    /// `Body.parse_target`.
    fn parse_target(&self, block: &[String]) -> (&'static str, String) {
        if let Some(last) = block.last()
            && strip(last).ends_with('_')
        {
            let reference = block.iter().map(|l| strip(l)).collect::<Vec<_>>().join(" ");
            if let Some(refname) = self.is_reference(&reference) {
                return ("refname", refname);
            }
        }
        let parts = split_escaped_whitespace(&block.join(" "));
        let reference = parts
            .iter()
            .map(|p| split_ws(&unescape(p, false)).join(""))
            .collect::<Vec<_>>()
            .join(" ");
        ("refuri", reference)
    }

    /// `Body.is_reference`.
    fn is_reference(&self, reference: &str) -> Option<String> {
        let m = self
            .pats
            .ex_reference
            .match_str(&whitespace_normalize_name(reference))?;
        let s = m
            .name("simple")
            .or_else(|| m.name("phrase"))
            .unwrap_or_default();
        Some(unescape(&s, false))
    }

    /// `Body.add_target`.
    fn add_target(
        &mut self,
        sm: &mut Sm,
        targetname: &str,
        refuri: &str,
        target: Id,
        lineno: isize,
    ) -> R<()> {
        self.doc.nm(target).line = Some(lineno.max(0) as usize);
        if !targetname.is_empty() {
            let name = fully_normalize_name(&unescape(targetname, false));
            self.doc.push_list(target, "names", &name);
            if !refuri.is_empty() {
                let uri = self.adjust_uri(refuri);
                self.doc.set_str(target, "refuri", &uri);
            }
            let parent = sm.node;
            self.note_explicit_target(target, Some(parent))?;
        } else {
            if !refuri.is_empty() {
                self.doc.set_str(target, "refuri", refuri);
            }
            self.doc.set(target, "anonymous", Val::Int(1));
            self.doc.note_anonymous_target(target);
        }
        Ok(())
    }

    /// `Body.substitution_def`.
    fn substitution_def(&mut self, sm: &mut Sm, m: &Match) -> R<(Vec<Id>, bool)> {
        let (src, srcline) = sm.get_source_and_line(None);
        let (block, _, offset, bf) =
            self.get_first_known_indented(sm, m.end(), false, false, true)?;
        let head: String = m.string()[..m.end()].iter().collect();
        let blocktext = format!("{}{}", head, block.data.join("\n"));
        let mut block = block;
        let mut offset = offset;
        let mut escaped = escape2null(rstrip(block.data.first().map(String::as_str).unwrap_or("")));
        let mut blockindex = 0;
        let subdefmatch = loop {
            if let Some(sm2) = self.pats.ex_substitution.match_str(&escaped) {
                break sm2;
            }
            blockindex += 1;
            match block.data.get(blockindex) {
                Some(b) => {
                    escaped = format!("{} {}", escaped, escape2null(strip(b)));
                }
                None => return Err(Flow::Markup("malformed substitution definition.".into())),
            }
        };
        block.trim_start(blockindex);
        let b0 = format!("{} ", strip(&block.data[0]));
        let start = subdefmatch.end() as isize - clen(&escaped) as isize - 1;
        let b0c: Vec<char> = b0.chars().collect();
        let n = b0c.len() as isize;
        let a = if start < 0 {
            (n + start).max(0)
        } else {
            start.min(n)
        } as usize;
        let b = (n - 1).max(0) as usize;
        block.data[0] = if a < b {
            b0c[a..b].iter().collect()
        } else {
            String::new()
        };
        if block.data[0].is_empty() {
            block.trim_start(1);
            offset += 1;
        }
        while !block.is_empty() && strip(block.data.last().unwrap()).is_empty() {
            block.trim_end(1);
        }
        let subname = subdefmatch.name("name").unwrap_or_default();
        let node = self.doc.elem(Tag::SubstitutionDefinition, &blocktext);
        self.doc.nm(node).source = src.clone();
        self.doc.nm(node).line = srcline;
        if block.is_empty() {
            let lb = self.literal(&blocktext);
            let msg = self.warning(
                &format!("Substitution definition \"{subname}\" missing contents."),
                MsgArgs {
                    source: src,
                    line: srcline.map(|l| l as isize),
                    children: vec![lb],
                    ..Default::default()
                },
            )?;
            return Ok((vec![msg], bf));
        }
        block.data[0] = strip(&block.data[0]).to_string();
        self.doc
            .push_list(node, "names", &whitespace_normalize_name(&subname));
        let (_, bf) = self.nested_list_parse(
            block,
            offset,
            node,
            St::SubstitutionDef,
            bf,
            None,
            false,
            &|_| {},
        )?;
        let mut i = 0;
        let children = self.doc.children(node).to_vec();
        for c in children {
            let t = self.doc.tag(c);
            if !(t.is_inline() || t == Tag::Text) {
                let ch = self.doc.children(node)[i];
                self.add(sm.node, ch);
                self.doc.nm(node).children.remove(i);
            } else {
                i += 1;
            }
        }
        for e in self.doc.findall(node, &|t| t != Tag::Text) {
            if !self.doc.list(e, "ids").is_empty()
                || (self.doc.tag(e) == Tag::Reference && self.doc.truthy(e, "anonymous"))
                || (self.doc.tag(e) == Tag::FootnoteReference && self.doc.truthy(e, "auto"))
            {
                let lb1 = self.literal(&format!("<{}>", self.doc.tag(e).name()));
                let lb2 = self.literal(&blocktext);
                let msg = self.error(
                    &format!(
                        "Substitution definition contains illegal element <{}>:",
                        self.doc.tag(e).name()
                    ),
                    MsgArgs {
                        source: src,
                        line: srcline.map(|l| l as isize),
                        children: vec![lb1, lb2],
                        ..Default::default()
                    },
                )?;
                return Ok((vec![msg], bf));
            }
        }
        if self.doc.children(node).is_empty() {
            let lb = self.literal(&blocktext);
            let msg = self.warning(
                &format!("Substitution definition \"{subname}\" empty or invalid."),
                MsgArgs {
                    source: src,
                    line: srcline.map(|l| l as isize),
                    children: vec![lb],
                    ..Default::default()
                },
            )?;
            return Ok((vec![msg], bf));
        }
        let parent = sm.node;
        self.note_substitution_def(node, &subname, Some(parent))?;
        Ok((vec![node], bf))
    }

    /// `document.note_substitution_def`.
    fn note_substitution_def(&mut self, subdef: Id, def_name: &str, msgnode: Option<Id>) -> R<()> {
        let name = whitespace_normalize_name(def_name);
        if let Some(&old) = self.doc.substitution_defs.get(&name) {
            let msg = self.error(
                &format!("Duplicate substitution definition name: \"{name}\"."),
                MsgArgs::base(subdef),
            )?;
            if let Some(m) = msgnode {
                self.add(m, msg);
            }
            self.doc.dupname(old, &name);
        }
        self.doc.substitution_defs.insert(name.clone(), subdef);
        self.doc
            .substitution_names
            .insert(fully_normalize_name(&name), name);
        Ok(())
    }

    /// `Body.anonymous_target`.
    fn anonymous_target(&mut self, sm: &mut Sm, m: &Match) -> R<(Vec<Id>, bool)> {
        let lineno = sm.abs_line_number();
        let (block, _, _, bf) = self.get_first_known_indented(sm, m.end(), true, true, true)?;
        let head: String = m.string()[..m.end()].iter().collect();
        let blocktext = format!("{}{}", head, block.data.join("\n"));
        let block: Vec<String> = block.data.iter().map(|l| escape2null(l)).collect();
        let target = self.make_target(sm, &block, &blocktext, lineno, "")?;
        Ok((vec![target], bf))
    }

    /// `MetaBody.parsemeta`.
    fn parsemeta(&mut self, sm: &mut Sm, m: &Match) -> R<Id> {
        let name = unescape(&escape2null(&parse_field_marker(m)), false);
        let (indented, _, _, _) = self.get_first_known_indented(sm, m.end(), false, true, true)?;
        let node = self.doc.elem(Tag::Meta, "");
        let content = unescape(&escape2null(&indented.data.join(" ")), false);
        self.doc.set_str(node, "content", &content);
        if indented.is_empty() {
            let line = sm.cur();
            let lb = self.literal(&line);
            return self.info(
                &format!("No content for meta tag \"{name}\"."),
                MsgArgs::default().child(lb),
            );
        }
        let tokens = split_ws(&name);
        match extract_name_value(tokens[0]) {
            Some((a, v)) => self.doc.set_str(node, &a.to_lowercase(), &v),
            None => self.doc.set_str(node, "name", tokens[0]),
        }
        for token in &tokens[1..] {
            match extract_name_value(token) {
                Some((a, v)) => self.doc.set_str(node, &a.to_lowercase(), &v),
                None => {
                    let line = sm.cur();
                    let lb = self.literal(&line);
                    return self.error(
                        &format!("Error parsing meta tag attribute \"{token}\": missing \"=\"."),
                        MsgArgs::default().child(lb),
                    );
                }
            }
        }
        Ok(node)
    }
}

/// `Body.check_attribution`.
fn check_attribution(indented: &StringList, start: usize) -> (Option<usize>, usize) {
    let mut indent: Option<usize> = None;
    let mut i = start + 1;
    let mut broke = false;
    for j in start + 1..indented.len() {
        i = j;
        let line = rstrip(&indented.data[j]);
        if line.is_empty() {
            broke = true;
            break;
        }
        let ind = clen(line) - clen(super::chars::lstrip(line));
        match indent {
            None => indent = Some(ind),
            Some(x) if x != ind => return (None, 0),
            _ => {}
        }
    }
    if !broke {
        i += 1;
    }
    (Some(i), indent.unwrap_or(0))
}

fn format_affixes(format: &str) -> (&'static str, &'static str) {
    match format {
        "parens" => ("(", ")"),
        "rparen" => ("", ")"),
        _ => ("", "."),
    }
}

/// `Body.make_enumerator`.
fn make_enumerator(ordinal: i64, sequence: &str, format: &str) -> Option<(String, String)> {
    let enumerator = if sequence == "#" {
        "#".to_string()
    } else if sequence == "arabic" {
        ordinal.to_string()
    } else {
        let e = if sequence.ends_with("alpha") {
            if ordinal > 26 {
                return None;
            }
            char::from_u32((ordinal + 'a' as i64 - 1) as u32)?.to_string()
        } else {
            to_roman(ordinal)?
        };
        if sequence.starts_with("lower") {
            e.to_lowercase()
        } else {
            e.to_uppercase()
        }
    };
    let (prefix, suffix) = format_affixes(format);
    Some((
        format!("{prefix}{enumerator}{suffix} "),
        format!("{prefix}#{suffix} "),
    ))
}

const ROMAN: [(&str, i64); 13] = [
    ("M", 1000),
    ("CM", 900),
    ("D", 500),
    ("CD", 400),
    ("C", 100),
    ("XC", 90),
    ("L", 50),
    ("XL", 40),
    ("X", 10),
    ("IX", 9),
    ("V", 5),
    ("IV", 4),
    ("I", 1),
];

/// `roman.toRoman`.
pub(super) fn to_roman(mut n: i64) -> Option<String> {
    if !(0 < n && n < 5000) {
        return None;
    }
    let mut s = String::new();
    for (r, v) in ROMAN {
        while n >= v {
            s.push_str(r);
            n -= v;
        }
    }
    Some(s)
}

/// `roman.fromRoman` (`None` for an invalid numeral).
fn from_roman(s: &str) -> Option<i64> {
    let re = Regex::new(
        "^M{0,4}(CM|CD|D?C{0,3})(XC|XL|L?X{0,3})(IX|IV|V?I{0,3})$",
        false,
    );
    if s.is_empty() || !re.is_match(s) {
        return None;
    }
    let mut result = 0;
    let mut index = 0;
    let cs: Vec<char> = s.chars().collect();
    for (r, v) in ROMAN {
        let rc: Vec<char> = r.chars().collect();
        while cs.len() >= index + rc.len() && cs[index..index + rc.len()] == rc[..] {
            result += v;
            index += rc.len();
        }
    }
    Some(result)
}

/// `Body.parse_field_marker`.
fn parse_field_marker(m: &Match) -> String {
    let g = m.group(0).unwrap();
    let field = slice_from(&g, 1);
    match field.rfind(':') {
        Some(i) => field[..i].to_string(),
        None => field.to_string(),
    }
}

/// `utils.escape2null`.
pub(super) fn escape2null(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push('\x00');
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `utils.split_escaped_whitespace`.
pub(super) fn split_escaped_whitespace(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for s in text.split("\x00 ") {
        for t in s.split("\x00\n") {
            out.push(t.to_string());
        }
    }
    out
}

/// `Text.classifier_delimiter.split` (`' +: +'`).
fn split_classifier(text: &str) -> Vec<String> {
    let cs: Vec<char> = text.chars().collect();
    let mut parts = Vec::new();
    let mut last = 0;
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == ' ' {
            let mut j = i;
            while j < cs.len() && cs[j] == ' ' {
                j += 1;
            }
            if j < cs.len() && cs[j] == ':' {
                let mut k = j + 1;
                while k < cs.len() && cs[k] == ' ' {
                    k += 1;
                }
                if k > j + 1 {
                    parts.push(cs[last..i].iter().collect());
                    last = k;
                    i = k;
                    continue;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    parts.push(cs[last..].iter().collect());
    parts
}

/// `utils.extract_name_value` for one token (`None` is `NameValueError`).
fn extract_name_value(token: &str) -> Option<(String, String)> {
    let (a, v) = token.split_once('=')?;
    if a.is_empty() {
        return None;
    }
    let v = v.trim_matches(|c| c == '"' || c == '\'');
    Some((a.to_string(), v.to_string()))
}

/// `re.escape`.
pub(super) fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if !c.is_alphanumeric() && c != '_' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
