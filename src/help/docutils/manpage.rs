//! `docutils/writers/manpage.py`: the `Translator` and `Table`.

use super::nodes::{Id, Tag, Val};
use super::parser::{MsgArgs, P, R, to_roman};

pub(super) const MACRO_DEF: &str = r#".
.nr rst2man-indent-level 0
.
.de1 rstReportMargin
\\$1 \\n[an-margin]
level \\n[rst2man-indent-level]
level margin: \\n[rst2man-indent\\n[rst2man-indent-level]]
-
\\n[rst2man-indent0]
\\n[rst2man-indent1]
\\n[rst2man-indent2]
..
.de1 INDENT
.\" .rstReportMargin pre:
. RS \\$1
. nr rst2man-indent\\n[rst2man-indent-level] \\n[an-margin]
. nr rst2man-indent-level +1
.\" .rstReportMargin post:
..
.de UNINDENT
. RE
.\" indent \\n[an-margin]
.\" old: \\n[rst2man-indent\\n[rst2man-indent-level]]
.nr rst2man-indent-level -1
.\" new: \\n[rst2man-indent\\n[rst2man-indent-level]]
.in \\n[rst2man-indent\\n[rst2man-indent-level]]u
..
"#;

/// `Table`.
struct Table {
    rows: Vec<Vec<Vec<String>>>,
    coldefs: Vec<&'static str>,
}

impl Table {
    fn append_cell(&mut self, cell_lines: Vec<String>) {
        let start = usize::from(cell_lines.first().is_some_and(|l| l == ".sp\n"));
        let row = self.rows.last_mut().unwrap();
        row.push(cell_lines[start.min(cell_lines.len())..].to_vec());
        if self.coldefs.len() < row.len() {
            self.coldefs.push("l");
        }
    }

    fn as_list(&mut self) -> Vec<String> {
        let mut text = vec![".TS\n".to_string(), "center;\n".to_string()];
        text.push(format!("|{}|.\n", self.coldefs.join("|")));
        for row in &mut self.rows {
            text.push("_\n".into());
            text.push("T{\n".into());
            let n = row.len();
            for (i, cell) in row.iter_mut().enumerate() {
                while cell.first().is_some_and(|l| l == "\n" || l == ".sp\n") {
                    cell.remove(0);
                }
                while cell.last().is_some_and(|l| l == "\n" || l == ".sp\n") {
                    cell.pop();
                }
                text.extend(cell.iter().cloned());
                if !text.last().unwrap().ends_with('\n') {
                    text.last_mut().unwrap().push('\n');
                }
                if i < n - 1 {
                    text.push("T}\tT{\n".into());
                } else {
                    text.push("T}\n".into());
                }
            }
        }
        text.push("_\n".into());
        text.push(".TE\n".into());
        text
    }
}

/// `EnumChar` of `Translator.list_start`.
struct EnumChar {
    style: String,
    cnt: i64,
    indent: usize,
}

impl EnumChar {
    fn next(&mut self) -> String {
        match self.style.as_str() {
            "bullet" => return "\\(bu".into(),
            "emdash" => return "\\(em".into(),
            _ => {}
        }
        self.cnt += 1;
        match self.style.as_str() {
            "arabic" => format!("{}.", self.cnt),
            "loweralpha" | "upperalpha" => {
                format!("{}.", char::from_u32(self.cnt as u32).unwrap_or('?'))
            }
            s if s.ends_with("roman") => {
                let r = format!("{}.", to_roman(self.cnt).unwrap_or_default());
                if s.starts_with("upper") {
                    r.to_uppercase()
                } else {
                    r.to_lowercase()
                }
            }
            _ => format!("{}.", self.cnt),
        }
    }
}

#[derive(Default)]
struct Docinfo {
    title: String,
    title_upper: String,
    subtitle: String,
    manual_section: String,
    manual_group: String,
    author: Vec<String>,
    date: String,
    copyright: String,
    version: String,
    other: Vec<(String, String)>,
}

/// `Translator`.
struct Tr {
    head: Vec<String>,
    body: Vec<String>,
    section_level: usize,
    context: Vec<Ctx>,
    list_char: Vec<EnumChar>,
    docinfo: Docinfo,
    docinfo_keys: Vec<String>,
    in_docinfo: bool,
    field_name: String,
    active_table: Option<Table>,
    in_literal: bool,
    header_written: bool,
    line_block: usize,
    indent: Vec<f64>,
}

enum Ctx {
    Int(usize),
    Str(String),
}

/// Outcome of a visit (`nodes.SkipNode`).
enum Visit {
    Go,
    Skip,
}

fn deunicode(text: &str) -> String {
    text.replace('\u{a0}', "\\ ").replace('\u{2020}', "\\(dg")
}

fn comment(text: &str) -> String {
    let mut out: String = text.split('\n').map(|l| format!(".\\\" {l}\n")).collect();
    out.push_str(".\n");
    out
}

fn fmt_indent(v: f64) -> String {
    format!(".INDENT {v:.1}\n")
}

impl Tr {
    fn ensure_eol(&mut self) {
        if let Some(last) = self.body.last()
            && !last.ends_with('\n')
        {
            self.body.push("\n".into());
        }
    }

    fn indent(&mut self, by: f64) {
        let step = *self.indent.last().unwrap();
        self.indent.push(by);
        self.body.push(fmt_indent(step));
    }

    fn dedent(&mut self) {
        self.indent.pop();
        self.body.push(".UNINDENT\n".into());
    }

    fn append_header(&mut self) {
        if self.header_written {
            return;
        }
        self.head.push(MACRO_DEF.into());
        let d = &self.docinfo;
        self.head.push(format!(
            ".TH \"{}\" {} \"{}\" \"{}\" \"{}\"\n.SH NAME\n{} \\- {}\n",
            d.title_upper, d.manual_section, d.date, d.version, d.manual_group, d.title, d.subtitle
        ));
        self.header_written = true;
    }

    fn astext(&mut self) -> String {
        if !self.header_written {
            self.append_header();
        }
        let b = &mut self.body;
        let get = |b: &Vec<String>, i: isize| -> String {
            let n = b.len() as isize;
            let j = if i < 0 { n + i } else { i };
            if j < 0 || j >= n {
                String::new()
            } else {
                b[j as usize].clone()
            }
        };
        for i in (1..b.len()).rev() {
            if b[i] == ".sp\n" {
                let p1 = get(b, i as isize - 1);
                let p2 = get(b, i as isize - 2);
                let p3 = get(b, i as isize - 3);
                if p1.starts_with(".BI ")
                    || p1.starts_with(".IP ")
                    || (p1.starts_with(".B ") && p2.starts_with(".TP\n"))
                    || (p1 == "\n"
                        && !is_roff_command(&p2)
                        && (p3.starts_with(".TP\n.B ") || p3.starts_with("\n.B ")))
                {
                    b[i] = ".\n".into();
                }
            }
        }
        let mut out = String::new();
        for s in self.head.iter().chain(self.body.iter()) {
            out.push_str(s);
        }
        out
    }
}

fn is_roff_command(s: &str) -> bool {
    let mut c = s.chars();
    c.next() == Some('.') && c.next().is_some_and(super::chars::is_word)
}

impl P {
    /// `Writer.translate`: the manpage text of the document.
    pub(super) fn write_manpage(&mut self) -> R<String> {
        let mut tr = Tr {
            head: Vec::new(),
            body: Vec::new(),
            section_level: 0,
            context: Vec::new(),
            list_char: Vec::new(),
            docinfo: Docinfo::default(),
            docinfo_keys: Vec::new(),
            in_docinfo: false,
            field_name: String::new(),
            active_table: None,
            in_literal: false,
            header_written: false,
            line_block: 0,
            indent: vec![0.0],
        };
        self.walkabout(&mut tr, 0)?;
        Ok(tr.astext())
    }

    fn walkabout(&mut self, tr: &mut Tr, node: Id) -> R<()> {
        if let Visit::Skip = self.visit(tr, node)? {
            return Ok(());
        }
        for c in self.doc.children(node).to_vec() {
            self.walkabout(tr, c)?;
        }
        self.depart(tr, node)
    }

    fn first_child(&self, node: Id) -> bool {
        let Some(parent) = self.doc.parent(node) else {
            return false;
        };
        let ch = self.doc.children(parent);
        let skip = usize::from(ch.first().is_some_and(|&c| self.doc.tag(c) == Tag::Label));
        for &c in &ch[skip.min(ch.len())..] {
            if self.doc.tag(c).is_invisible() {
                continue;
            }
            return c == node;
        }
        false
    }

    fn list_start(&mut self, tr: &mut Tr, node: Id) {
        let style = self
            .doc
            .get_str(node, "enumtype")
            .unwrap_or_else(|| "bullet".into());
        let mut cnt = match self.doc.get(node, "start") {
            Some(Val::Int(s)) => s - 1,
            _ => 0,
        };
        let mut indent = 2;
        if style == "arabic" {
            indent = self.doc.children(node).len().to_string().len() + cnt.to_string().len() + 1;
        } else if style == "loweralpha" {
            cnt += 'a' as i64 - 1;
            indent = 3;
        } else if style == "upperalpha" {
            cnt += 'A' as i64 - 1;
            indent = 3;
        } else if style.ends_with("roman") {
            indent = 5;
        }
        tr.list_char.push(EnumChar { style, cnt, indent });
        let n = tr.list_char.len();
        let w = if n > 1 {
            tr.list_char[n - 2].indent
        } else {
            tr.list_char[n - 1].indent
        };
        tr.indent(w as f64);
    }

    fn visit_admonition(&mut self, tr: &mut Tr, name: &str) {
        let label = match name {
            "attention" => "Attention!",
            "caution" => "Caution!",
            "danger" => "!DANGER!",
            "error" => "Error",
            "hint" => "Hint",
            "important" => "Important",
            "note" => "Note",
            "tip" => "Tip",
            "warning" => "Warning",
            other => other,
        };
        tr.body.push(".sp\n".into());
        tr.body.push(format!("\\fB{}:\\fP\n", label.to_uppercase()));
        tr.indent(3.5);
        tr.indent(0.0);
    }

    fn docinfo_item(&mut self, tr: &mut Tr, node: Id, name: &str) -> Visit {
        let text = self.doc.astext(node);
        match name {
            "author" => tr.docinfo.author.push(text),
            "title" => tr.docinfo.title = text,
            "subtitle" => tr.docinfo.subtitle = text,
            "date" => tr.docinfo.date = text,
            "copyright" => tr.docinfo.copyright = text,
            "version" => tr.docinfo.version = text,
            _ => {
                tr.docinfo.other.retain(|(k, _)| k != name);
                tr.docinfo.other.push((name.to_string(), text));
            }
        }
        tr.docinfo_keys.push(name.to_string());
        Visit::Skip
    }

    fn visit(&mut self, tr: &mut Tr, node: Id) -> R<Visit> {
        let tag = self.doc.tag(node);
        let parent_tag = self.doc.parent(node).map(|p| self.doc.tag(p));
        match tag {
            Tag::Text => {
                let mut text = self.doc.astext(node);
                text = text.replace('\\', "\\e");
                for (a, b) in [
                    ("-", "\\-"),
                    ("'", "\\(aq"),
                    ("\u{b4}", "\\'"),
                    ("`", "\\(ga"),
                    ("\"", "\\(dq"),
                ] {
                    text = text.replace(a, b);
                }
                text = deunicode(&text);
                if text.starts_with('.') {
                    text = format!("\\&{text}");
                }
                if tr.in_literal {
                    text = text.replace("\n.", "\n\\&.");
                }
                tr.body.push(text);
            }
            Tag::Document => {
                tr.head.push(format!(
                    "{}\n",
                    comment("Man page generated from reStructuredText.").trim_end()
                ));
                tr.header_written = false;
            }
            Tag::Section => tr.section_level += 1,
            Tag::Title => match parent_tag {
                Some(Tag::Admonition) => tr.body.push(".IP \"".into()),
                _ if tr.section_level == 0 => {
                    let t = self.doc.astext(node);
                    tr.docinfo.title_upper = t.to_uppercase();
                    tr.docinfo.title = t;
                    return Ok(Visit::Skip);
                }
                _ if tr.section_level == 1 => {
                    let t = self.doc.astext(node).to_uppercase();
                    tr.body.push(format!(".SH {}\n", deunicode(&t)));
                    return Ok(Visit::Skip);
                }
                _ => tr.body.push(".SS ".into()),
            },
            Tag::Subtitle => match parent_tag {
                Some(Tag::Section) => tr.body.push("\\fB".into()),
                Some(Tag::Document) => return Ok(self.docinfo_item(tr, node, "subtitle")),
                _ => {}
            },
            Tag::Paragraph => {
                tr.ensure_eol();
                if !self.first_child(node) {
                    tr.body.push(".sp\n".into());
                }
                tr.in_literal = true;
            }
            Tag::Emphasis | Tag::TitleReference => tr.body.push("\\fI".into()),
            Tag::Strong => tr.body.push("\\fB".into()),
            Tag::Literal => tr.body.push("\\fB".into()),
            Tag::Reference => tr.body.push("\\fI\\%".into()),
            Tag::LiteralBlock | Tag::DoctestBlock => {
                if tag == Tag::LiteralBlock {
                    tr.indent(3.5);
                    tr.indent(0.0);
                }
                tr.body.push(".sp\n.nf\n.ft C\n".into());
                tr.in_literal = true;
            }
            Tag::BulletList | Tag::EnumeratedList => self.list_start(tr, node),
            Tag::ListItem => {
                let lc = tr.list_char.last_mut().unwrap();
                let s = lc.next();
                let w = lc.indent;
                tr.body.push(format!(".IP {s} {w}\n"));
            }
            Tag::DefinitionList => tr.indent(7.0),
            Tag::DefinitionListItem => tr.body.push(".TP".into()),
            Tag::Term => tr.body.push("\n.B ".into()),
            Tag::FieldList => tr.indent(7.0),
            Tag::FieldName => {
                if tr.in_docinfo {
                    tr.field_name = self.doc.astext(node);
                    return Ok(Visit::Skip);
                }
                tr.body.push(".TP\n.B ".into());
            }
            Tag::FieldBody => {
                if tr.in_docinfo {
                    let name = tr.field_name.to_lowercase().replace(' ', "_");
                    return Ok(self.docinfo_item(tr, node, &name));
                }
            }
            Tag::OptionList => tr.indent(7.0),
            Tag::OptionListItem => tr.body.push(".TP\n".into()),
            Tag::OptionGroup => {
                tr.context.push(Ctx::Str(".B ".into()));
                tr.context.push(Ctx::Int(tr.body.len()));
                tr.context.push(Ctx::Int(0));
            }
            Tag::Option => {
                let n = tr.context.len();
                let bi = matches!(&tr.context[n - 3], Ctx::Str(s) if s == ".BI");
                if matches!(tr.context[n - 1], Ctx::Int(c) if c > 0) {
                    tr.body.push(if bi {
                        "\\fR,\\fB ".into()
                    } else {
                        "\\fP,\\fB ".into()
                    });
                }
                if bi {
                    tr.body.push("\\".into());
                }
                tr.body.push(" ".into());
            }
            Tag::OptionArgument => {
                let n = tr.context.len();
                tr.context[n - 3] = Ctx::Str(".BI".into());
                let delim = self.doc.get_str(node, "delimiter").unwrap_or_default();
                if delim != " " {
                    tr.body.push(format!("\\fB{delim} "));
                } else if tr.body.last().is_some_and(|l| l.ends_with('=')) {
                    tr.body.push(" ".into());
                } else {
                    tr.body.push(" \\ ".into());
                }
            }
            Tag::BlockQuote => {
                tr.indent(3.5);
                tr.indent(0.0);
            }
            Tag::Attention
            | Tag::Caution
            | Tag::Danger
            | Tag::Error
            | Tag::Hint
            | Tag::Important
            | Tag::Note
            | Tag::Tip
            | Tag::Warning => self.visit_admonition(tr, tag.name()),
            Tag::Admonition => {
                tr.indent(3.5);
                tr.indent(0.0);
            }
            Tag::Comment => {
                let t = self.doc.astext(node);
                tr.body.push(comment(&t));
                return Ok(Visit::Skip);
            }
            Tag::Target | Tag::SubstitutionDefinition => return Ok(Visit::Skip),
            Tag::SystemMessage => {
                let line = match self.doc.get(node, "line") {
                    Some(v) => format!(", line {}", v.as_str()),
                    None => String::new(),
                };
                tr.body.push(format!(
                    ".IP \"System Message: {}/{} ({}:{})\"\n",
                    self.doc.get_str(node, "type").unwrap_or_default(),
                    self.doc.get_str(node, "level").unwrap_or_default(),
                    self.doc.get_str(node, "source").unwrap_or_default(),
                    line
                ));
            }
            Tag::Problematic => tr.body.push("\n.nf\n".into()),
            Tag::Transition => tr.body.push("\n.sp\n.ce\n----\n".into()),
            Tag::LineBlock => {
                tr.line_block += 1;
                if tr.line_block == 1 {
                    tr.body.push(".nf\n".into());
                } else {
                    tr.body.push(".in +2\n".into());
                }
            }
            Tag::Attribution => tr.body.push("\\(em ".into()),
            Tag::Table => {
                tr.active_table = Some(Table {
                    rows: Vec::new(),
                    coldefs: Vec::new(),
                });
            }
            Tag::Row => tr.active_table.as_mut().unwrap().rows.push(Vec::new()),
            Tag::Entry => {
                if self.doc.has(node, "morerows") {
                    self.warning("\"table row spanning\" not supported", MsgArgs::base(node))?;
                }
                if self.doc.has(node, "morecols") {
                    self.warning("\"table cell spanning\" not supported", MsgArgs::base(node))?;
                }
                tr.context.push(Ctx::Int(tr.body.len()));
            }
            Tag::Footnote => {
                let t = self.doc.astext(node);
                let num = t.split_whitespace().next().unwrap_or("").to_string();
                tr.body.push(format!(".IP [{}] 5\n", deunicode(num.trim())));
            }
            Tag::Citation => {
                let t = self.doc.astext(node);
                let num = t.split_whitespace().next().unwrap_or("").to_string();
                tr.body.push(format!(".IP [{}] 5\n", num.trim()));
            }
            Tag::FootnoteReference => {
                tr.body
                    .push(format!("[{}]", deunicode(&self.doc.astext(node))));
                return Ok(Visit::Skip);
            }
            Tag::CitationReference => {
                tr.body.push(format!("[{}]", self.doc.astext(node)));
                return Ok(Visit::Skip);
            }
            Tag::Label => {
                if matches!(parent_tag, Some(Tag::Footnote | Tag::Citation)) {
                    return Ok(Visit::Skip);
                }
                self.warning("\"unsupported \"label\"", MsgArgs::base(node))?;
                tr.body.push("[".into());
            }
            Tag::Subscript => tr.body.push("\\s-2\\d".into()),
            Tag::Superscript => tr.body.push("\\s-2\\u".into()),
            Tag::Math => {
                self.warning("\"math\" role not supported", MsgArgs::base(node))?;
                tr.body.push("\\fB".into());
            }
            Tag::MathBlock => {
                self.warning("\"math\" directive not supported", MsgArgs::base(node))?;
                tr.indent(3.5);
                tr.indent(0.0);
                tr.body.push(".sp\n.nf\n.ft C\n".into());
                tr.in_literal = true;
            }
            Tag::SubstitutionReference => {
                self.warning(
                    "\"substitution_reference\" not supported",
                    MsgArgs::base(node),
                )?;
            }
            _ => {}
        }
        Ok(Visit::Go)
    }

    fn depart(&mut self, tr: &mut Tr, node: Id) -> R<()> {
        let tag = self.doc.tag(node);
        let parent_tag = self.doc.parent(node).map(|p| self.doc.tag(p));
        match tag {
            Tag::Document => {
                if !tr.docinfo.author.is_empty() {
                    tr.body
                        .push(format!(".SH AUTHOR\n{}\n", tr.docinfo.author.join(", ")));
                }
                for name in tr.docinfo_keys.clone() {
                    if [
                        "author",
                        "copyright",
                        "date",
                        "manual_group",
                        "manual_section",
                        "subtitle",
                        "title",
                        "title_upper",
                        "version",
                    ]
                    .contains(&name.as_str())
                    {
                        continue;
                    }
                    let value = tr
                        .docinfo
                        .other
                        .iter()
                        .find(|(k, _)| *k == name)
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    tr.body.push(format!("\n{name}: {value}\n"));
                }
                if !tr.docinfo.copyright.is_empty() {
                    tr.body
                        .push(format!(".SH COPYRIGHT\n{}\n", tr.docinfo.copyright));
                }
                tr.body
                    .push(comment("Generated by docutils manpage writer."));
            }
            Tag::Section => tr.section_level -= 1,
            Tag::Title => {
                if parent_tag == Some(Tag::Admonition) {
                    tr.body.push("\"".into());
                }
                tr.body.push("\n".into());
            }
            Tag::Subtitle => tr.body.push("\\fP\n.PP\n".into()),
            Tag::Paragraph => {
                tr.in_literal = false;
                tr.body.push("\n".into());
            }
            Tag::Emphasis
            | Tag::TitleReference
            | Tag::Strong
            | Tag::Literal
            | Tag::Reference
            | Tag::Math => tr.body.push("\\fP".into()),
            Tag::LiteralBlock | Tag::MathBlock => {
                tr.in_literal = false;
                tr.body.push("\n.ft P\n.fi\n".into());
                tr.dedent();
                tr.dedent();
            }
            Tag::DoctestBlock => {
                tr.in_literal = false;
                tr.body.push("\n.ft P\n.fi\n".into());
            }
            Tag::BulletList | Tag::EnumeratedList => {
                tr.dedent();
                tr.list_char.pop();
            }
            Tag::DefinitionList | Tag::FieldList | Tag::OptionList => tr.dedent(),
            Tag::Term => tr.body.push("\n".into()),
            Tag::FieldName => tr.body.push("\n".into()),
            Tag::OptionGroup => {
                tr.context.pop();
                let Some(Ctx::Int(start)) = tr.context.pop() else {
                    unreachable!()
                };
                let text: String = tr.body.drain(start..).collect();
                let Some(Ctx::Str(prefix)) = tr.context.pop() else {
                    unreachable!()
                };
                tr.body.push(format!("{prefix}{text}\n"));
            }
            Tag::Option => {
                if let Some(Ctx::Int(c)) = tr.context.last_mut() {
                    *c += 1;
                }
            }
            Tag::BlockQuote | Tag::Admonition => {
                tr.dedent();
                tr.dedent();
            }
            Tag::Attention
            | Tag::Caution
            | Tag::Danger
            | Tag::Error
            | Tag::Hint
            | Tag::Important
            | Tag::Note
            | Tag::Tip
            | Tag::Warning => {
                tr.dedent();
                tr.dedent();
            }
            Tag::Problematic => tr.body.push("\n.fi\n".into()),
            Tag::Transition => tr.body.push("\n.ce 0\n.sp\n".into()),
            Tag::LineBlock => {
                tr.line_block -= 1;
                if tr.line_block == 0 {
                    tr.body.push(".fi\n".into());
                    tr.body.push(".sp\n".into());
                } else {
                    tr.body.push(".in -2\n".into());
                }
            }
            Tag::Line | Tag::Attribution => tr.body.push("\n".into()),
            Tag::Table => {
                tr.ensure_eol();
                let mut t = tr.active_table.take().unwrap();
                tr.body.extend(t.as_list());
            }
            Tag::Entry => {
                let Some(Ctx::Int(start)) = tr.context.pop() else {
                    unreachable!()
                };
                let cell: Vec<String> = tr.body.drain(start..).collect();
                tr.active_table.as_mut().unwrap().append_cell(cell);
            }
            Tag::Label => tr.body.push("]\n".into()),
            Tag::Subscript => tr.body.push("\\u\\s0".into()),
            Tag::Superscript => tr.body.push("\\d\\s0".into()),
            _ => {}
        }
        Ok(())
    }
}
