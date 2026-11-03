//! Directives (`Body.directive`, `Body.run_directive`,
//! `docutils/parsers/rst/directives/`): admonitions, `code`, `meta`.

use super::chars::{split_ws, split_ws_max, strip};
use super::nodes::{Id, Tag, Val, fully_normalize_name, make_id};
use super::parser::{Flow, MsgArgs, P, R, Sm, St};
use super::pyre::Match;
use super::statemachine::StringList;

/// The directive classes this port implements.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Admonition(Tag),
    Code,
    Meta,
    Replace,
    Math,
}

struct Spec {
    required: usize,
    optional: usize,
    final_ws: bool,
    options: &'static [&'static str],
    has_content: bool,
}

fn spec(kind: Kind) -> Spec {
    match kind {
        Kind::Admonition(Tag::Admonition) => Spec {
            required: 1,
            optional: 0,
            final_ws: true,
            options: &["class", "name"],
            has_content: true,
        },
        Kind::Admonition(_) => Spec {
            required: 0,
            optional: 0,
            final_ws: true,
            options: &["class", "name"],
            has_content: true,
        },
        Kind::Code => Spec {
            required: 0,
            optional: 1,
            final_ws: false,
            options: &["class", "name", "number-lines"],
            has_content: true,
        },
        Kind::Meta => Spec {
            required: 0,
            optional: 0,
            final_ws: false,
            options: &[],
            has_content: true,
        },
        Kind::Replace => Spec {
            required: 0,
            optional: 0,
            final_ws: false,
            options: &[],
            has_content: true,
        },
        Kind::Math => Spec {
            required: 0,
            optional: 0,
            final_ws: false,
            options: &["class", "name"],
            has_content: true,
        },
    }
}

/// `en.directives` → `_directive_registry` for the implemented classes;
/// `Err(true)` for a directive that exists but is not ported.
fn lookup(normname: &str) -> Result<Kind, bool> {
    Ok(match normname {
        "attention" => Kind::Admonition(Tag::Attention),
        "caution" => Kind::Admonition(Tag::Caution),
        "danger" => Kind::Admonition(Tag::Danger),
        "error" => Kind::Admonition(Tag::Error),
        "hint" => Kind::Admonition(Tag::Hint),
        "important" => Kind::Admonition(Tag::Important),
        "note" => Kind::Admonition(Tag::Note),
        "tip" => Kind::Admonition(Tag::Tip),
        "warning" => Kind::Admonition(Tag::Warning),
        "admonition" => Kind::Admonition(Tag::Admonition),
        "code" | "code-block" | "sourcecode" => Kind::Code,
        "meta" => Kind::Meta,
        "replace" => Kind::Replace,
        "math" => Kind::Math,
        "sidebar"
        | "topic"
        | "line-block"
        | "parsed-literal"
        | "rubric"
        | "epigraph"
        | "highlights"
        | "pull-quote"
        | "compound"
        | "container"
        | "table"
        | "csv-table"
        | "list-table"
        | "image"
        | "figure"
        | "include"
        | "raw"
        | "unicode"
        | "date"
        | "class"
        | "role"
        | "default-role"
        | "title"
        | "contents"
        | "sectnum"
        | "section-numbering"
        | "header"
        | "footer"
        | "target-notes"
        | "restructuredtext-test-directive" => return Err(true),
        _ => return Err(false),
    })
}

/// A parsed directive block.
struct Block {
    arguments: Vec<String>,
    options: Vec<(String, Option<String>)>,
    content: StringList,
    content_offset: isize,
}

/// `DirectiveError`.
struct DirectiveError(usize, String);

impl P {
    /// `Body.directive`.
    pub(super) fn directive(
        &mut self,
        sm: &mut Sm,
        m: &Match,
        alt: Option<String>,
    ) -> R<(Vec<Id>, bool)> {
        let type_name = m.group(1).unwrap();
        let normname = type_name.to_lowercase();
        match lookup(&normname) {
            Ok(kind) => self.run_directive(sm, m, &type_name, kind, alt.is_some()),
            Err(known) => {
                if !known {
                    let line = self.doc.current_line.map(|l| l as isize);
                    let msg = self.info(
                        &format!(
                            "No directive entry for \"{type_name}\" in module \"docutils.parsers.rst.languages.en\".\nTrying \"{type_name}\" as canonical directive name."
                        ),
                        MsgArgs {
                            line,
                            ..Default::default()
                        },
                    )?;
                    let parent = sm.node;
                    self.doc.append(parent, msg);
                }
                self.unknown_directive(sm, &type_name)
            }
        }
    }

    /// `Body.unknown_directive`.
    fn unknown_directive(&mut self, sm: &mut Sm, type_name: &str) -> R<(Vec<Id>, bool)> {
        let lineno = sm.abs_line_number();
        let (indented, _, _, bf) = self.get_first_known_indented(sm, 0, false, false, true)?;
        let text = indented.data.join("\n");
        let lb = self.literal(&text);
        let e = self.error(
            &format!("Unknown directive type \"{type_name}\"."),
            MsgArgs::line(lineno).child(lb),
        )?;
        Ok((vec![e], bf))
    }

    /// `Body.run_directive`.
    fn run_directive(
        &mut self,
        sm: &mut Sm,
        m: &Match,
        type_name: &str,
        kind: Kind,
        in_subdef: bool,
    ) -> R<(Vec<Id>, bool)> {
        let lineno = sm.abs_line_number();
        let initial = sm.line_offset.max(0) as usize;
        let (indented, _, line_offset, bf) =
            self.get_first_known_indented(sm, m.end(), false, true, false)?;
        let end = (sm.line_offset + 1).max(0) as usize;
        let block_text = sm.lines.slice(initial, end).data.join("\n");
        let block = match self.parse_directive_block(indented, line_offset, kind) {
            Ok(b) => b,
            Err(Flow::Markup(detail)) => {
                let lb = self.literal(&block_text);
                let e = self.error(
                    &format!("Error in \"{type_name}\" directive:\n{detail}."),
                    MsgArgs::line(lineno).child(lb),
                )?;
                return Ok((vec![e], bf));
            }
            Err(e) => return Err(e),
        };
        let result = match kind {
            Kind::Admonition(tag) => self.admonition(sm, tag, type_name, lineno, block)?,
            Kind::Code => self.code_block(type_name, block)?,
            Kind::Meta => self.meta(lineno, &block_text, block)?,
            Kind::Replace => self.replace(sm, type_name, lineno, in_subdef, block)?,
            Kind::Math => self.math_block(sm, type_name, lineno, &block_text, block)?,
        };
        let result = match result {
            Ok(nodes) => nodes,
            Err(DirectiveError(level, msg)) => {
                let node = self.system_message(level, &msg, MsgArgs::line(lineno))?;
                let lb = self.literal(&block_text);
                self.doc.append(node, lb);
                vec![node]
            }
        };
        Ok((result, bf || sm.is_next_line_blank()))
    }

    /// `Body.parse_directive_block`.
    fn parse_directive_block(
        &mut self,
        mut indented: StringList,
        mut line_offset: isize,
        kind: Kind,
    ) -> R<Block> {
        let sp = spec(kind);
        if !indented.is_empty() && strip(&indented.data[0]).is_empty() {
            indented.trim_start(1);
            line_offset += 1;
        }
        while !indented.is_empty() && strip(indented.data.last().unwrap()).is_empty() {
            indented.trim_end(1);
        }
        let mut i = 0;
        let (mut arg_block, mut content, mut content_offset);
        if !indented.is_empty() && (sp.required > 0 || sp.optional > 0 || !sp.options.is_empty()) {
            i = indented.len();
            for (j, line) in indented.data.iter().enumerate() {
                if strip(line).is_empty() {
                    i = j;
                    break;
                }
            }
            arg_block = indented.slice(0, i);
            content = indented.slice_from(i + 1);
            content_offset = line_offset + i as isize + 1;
        } else {
            content = indented.clone();
            content_offset = line_offset;
            arg_block = StringList::default();
        }
        let mut options = Vec::new();
        if !sp.options.is_empty() {
            let pos = arg_block
                .data
                .iter()
                .position(|l| self.pats.field_marker.is_match(l));
            let opt_block = match pos {
                Some(p) => {
                    let ob = arg_block.slice_from(p);
                    arg_block = arg_block.slice(0, p);
                    ob
                }
                None => StringList::default(),
            };
            if !opt_block.is_empty() {
                options = self.parse_extension_options(sp.options, opt_block)?;
            }
        }
        if !arg_block.is_empty() && !(sp.required > 0 || sp.optional > 0) {
            let mut c = arg_block.clone();
            let rest = indented.slice_from(i);
            c.data.extend(rest.data);
            c.items.extend(rest.items);
            content = c;
            content_offset = line_offset;
            arg_block = StringList::default();
        }
        while !content.is_empty() && strip(&content.data[0]).is_empty() {
            content.trim_start(1);
            content_offset += 1;
        }
        let arguments = if sp.required > 0 || sp.optional > 0 {
            let arg_text = arg_block.data.join("\n");
            let mut arguments: Vec<String> =
                split_ws(&arg_text).iter().map(|s| s.to_string()).collect();
            if arguments.len() < sp.required {
                return Err(Flow::Markup(format!(
                    "{} argument(s) required, {} supplied",
                    sp.required,
                    arguments.len()
                )));
            } else if arguments.len() > sp.required + sp.optional {
                if sp.final_ws {
                    arguments = split_ws_max(&arg_text, sp.required + sp.optional - 1);
                } else {
                    return Err(Flow::Markup(format!(
                        "maximum {} argument(s) allowed, {} supplied",
                        sp.required + sp.optional,
                        arguments.len()
                    )));
                }
            }
            arguments
        } else {
            Vec::new()
        };
        if !content.is_empty() && !sp.has_content {
            return Err(Flow::Markup("no content permitted".into()));
        }
        Ok(Block {
            arguments,
            options,
            content,
            content_offset,
        })
    }

    /// `Body.parse_extension_options` with `utils.extract_extension_options`.
    fn parse_extension_options(
        &mut self,
        spec: &[&str],
        datalines: StringList,
    ) -> R<Vec<(String, Option<String>)>> {
        let node = self.doc.elem(Tag::FieldList, "");
        let n = datalines.len() as isize;
        let (new_off, bf) = self.nested_list_parse(
            datalines,
            0,
            node,
            St::ExtensionOptions,
            true,
            None,
            false,
            &|_| {},
        )?;
        if new_off != n {
            return Err(Flow::Markup("invalid option block".into()));
        }
        let mut options: Vec<(String, Option<String>)> = Vec::new();
        for field in self.doc.children(node).to_vec() {
            let ch = self.doc.children(field).to_vec();
            let fname = self.doc.astext(ch[0]);
            if split_ws(&fname).len() != 1 {
                return Err(Flow::Markup(
                    "invalid option data: extension option field name may not contain multiple words".into(),
                ));
            }
            let name = fname.to_lowercase();
            let body = ch[1];
            let bch = self.doc.children(body).to_vec();
            let data = if bch.is_empty() {
                None
            } else if bch.len() > 1
                || self.doc.tag(bch[0]) != Tag::Paragraph
                || self.doc.children(bch[0]).len() != 1
                || self.doc.tag(self.doc.children(bch[0])[0]) != Tag::Text
            {
                return Err(Flow::Markup(format!(
                    "invalid option data: extension option field body may contain\na single paragraph only (option \"{name}\")"
                )));
            } else {
                Some(self.doc.astext(self.doc.children(bch[0])[0]))
            };
            if !spec.contains(&name.as_str()) {
                return Err(Flow::Markup(format!("unknown option: \"{name}\"")));
            }
            if options.iter().any(|(n, _)| *n == name) {
                return Err(Flow::Markup(format!(
                    "invalid option data: duplicate option \"{name}\""
                )));
            }
            if name == "class" {
                let v = data.clone().ok_or_else(|| {
                    Flow::Markup(format!(
                        "invalid option value: (option: \"{name}\"; value: None)\nargument required but none supplied"
                    ))
                })?;
                for w in split_ws(&v) {
                    if make_id(w).is_empty() {
                        return Err(Flow::Markup(format!(
                            "invalid option value: (option: \"{name}\"; value: '{v}')\ncannot make \"{w}\" into a class name"
                        )));
                    }
                }
            }
            options.push((name, data));
        }
        if !bf {
            return Err(Flow::Markup("option data incompletely parsed".into()));
        }
        Ok(options)
    }

    fn apply_options(&mut self, node: Id, options: &[(String, Option<String>)]) -> R<()> {
        for (name, value) in options {
            match name.as_str() {
                "class" => {
                    let classes: Vec<String> = split_ws(value.as_deref().unwrap_or(""))
                        .iter()
                        .map(|w| make_id(w))
                        .collect();
                    let mut cur = self.doc.list(node, "classes");
                    cur.extend(classes);
                    self.doc.set(node, "classes", Val::List(cur));
                }
                "name" => {
                    let name = fully_normalize_name(value.as_deref().unwrap_or(""));
                    self.doc.push_list(node, "names", &name);
                    self.note_explicit_target(node, Some(node))?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `BaseAdmonition.run`.
    fn admonition(
        &mut self,
        sm: &mut Sm,
        tag: Tag,
        name: &str,
        lineno: isize,
        b: Block,
    ) -> R<Result<Vec<Id>, DirectiveError>> {
        if b.content.is_empty() {
            return Ok(Err(DirectiveError(
                3,
                format!("Content block expected for the \"{name}\" directive; none found."),
            )));
        }
        let text = b.content.data.join("\n");
        let node = self.doc.elem(tag, &text);
        let classes: Vec<(String, Option<String>)> = b
            .options
            .iter()
            .filter(|(n, _)| n == "class")
            .cloned()
            .collect();
        self.apply_options(node, &classes)?;
        let names: Vec<(String, Option<String>)> = b
            .options
            .iter()
            .filter(|(n, _)| n == "name")
            .cloned()
            .collect();
        self.apply_options(node, &names)?;
        if tag == Tag::Admonition {
            let title_text = b.arguments[0].clone();
            let (textnodes, messages) = self.inline_text(sm, &title_text, lineno)?;
            let title = self.doc.elem(Tag::Title, &title_text);
            self.doc.extend(title, &textnodes);
            let (s, l) = sm.get_source_and_line(Some(lineno));
            self.doc.nm(title).source = s;
            self.doc.nm(title).line = l;
            self.doc.append(node, title);
            self.doc.extend(node, &messages);
            if classes.is_empty() {
                self.doc.push_list(
                    node,
                    "classes",
                    &format!("admonition-{}", make_id(&title_text)),
                );
            }
        }
        self.nested_parse(b.content, b.content_offset, node, false)?;
        Ok(Ok(vec![node]))
    }

    /// `CodeBlock.run` (without Pygments).
    fn code_block(&mut self, name: &str, b: Block) -> R<Result<Vec<Id>, DirectiveError>> {
        if b.content.is_empty() {
            return Ok(Err(DirectiveError(
                3,
                format!("Content block expected for the \"{name}\" directive; none found."),
            )));
        }
        let language = b.arguments.first().cloned().unwrap_or_default();
        if !language.is_empty() && language != "text" {
            return Ok(Err(DirectiveError(
                2,
                "Cannot analyze code. Pygments package not found.".into(),
            )));
        }
        let text = b.content.data.join("\n");
        let node = self.doc.elem(Tag::LiteralBlock, &text);
        let mut classes = vec!["code".to_string()];
        if !language.is_empty() {
            classes.push(language);
        }
        for (n, v) in &b.options {
            if n == "class" {
                classes.extend(
                    split_ws(v.as_deref().unwrap_or(""))
                        .iter()
                        .map(|w| make_id(w)),
                );
            }
        }
        self.doc.set(node, "classes", Val::List(classes));
        let names: Vec<(String, Option<String>)> = b
            .options
            .iter()
            .filter(|(n, _)| n == "name")
            .cloned()
            .collect();
        self.apply_options(node, &names)?;
        if !text.is_empty() {
            let t = self.doc.text(&text);
            self.doc.append(node, t);
        }
        Ok(Ok(vec![node]))
    }

    /// `Meta.run`.
    fn meta(
        &mut self,
        lineno: isize,
        block_text: &str,
        b: Block,
    ) -> R<Result<Vec<Id>, DirectiveError>> {
        if b.content.is_empty() {
            return Ok(Err(DirectiveError(
                3,
                "Content block expected for the \"meta\" directive; none found.".into(),
            )));
        }
        let node = self.doc.elem(Tag::Element, "");
        let n = b.content.len() as isize;
        let (new_off, _) = self.nested_list_parse(
            b.content,
            b.content_offset,
            node,
            St::MetaBody,
            true,
            None,
            false,
            &|_| {},
        )?;
        if new_off - b.content_offset != n {
            let lb = self.literal(block_text);
            let e = self.error("Invalid meta directive.", MsgArgs::line(lineno).child(lb))?;
            self.doc.append(node, e);
        }
        let index = self
            .doc
            .first_child_not_matching(0, &|t| t.is_titular() || t == Tag::Meta)
            .unwrap_or(0);
        let children = self.doc.children(node).to_vec();
        self.doc.insert_many(0, index, &children);
        Ok(Ok(Vec::new()))
    }
}

impl P {
    /// `misc.Replace.run`.
    fn replace(
        &mut self,
        _sm: &mut Sm,
        name: &str,
        lineno: isize,
        in_subdef: bool,
        b: Block,
    ) -> R<Result<Vec<Id>, DirectiveError>> {
        if !in_subdef {
            return Ok(Err(DirectiveError(
                3,
                format!(
                    "Invalid context: the \"{name}\" directive can only be used within a substitution definition."
                ),
            )));
        }
        if b.content.is_empty() {
            return Ok(Err(DirectiveError(
                3,
                format!("Content block expected for the \"{name}\" directive; none found."),
            )));
        }
        let text = b.content.data.join("\n");
        let element = self.doc.elem(Tag::Element, &text);
        self.nested_parse(b.content, b.content_offset, element, false)?;
        let mut node = None;
        let mut messages = Vec::new();
        for elem in self.doc.children(element).to_vec() {
            let t = self.doc.tag(elem);
            if node.is_none() && t == Tag::Paragraph {
                node = Some(elem);
            } else if t == Tag::SystemMessage {
                self.doc.set(elem, "backrefs", Val::List(Vec::new()));
                messages.push(elem);
            } else {
                let e = self.error(
                    &format!("Error in \"{name}\" directive: may contain a single paragraph only."),
                    MsgArgs::line(lineno),
                )?;
                return Ok(Ok(vec![e]));
            }
        }
        if let Some(n) = node {
            messages.extend(self.doc.children(n).to_vec());
        }
        Ok(Ok(messages))
    }

    /// `body.MathBlock.run`.
    fn math_block(
        &mut self,
        _sm: &mut Sm,
        name: &str,
        _lineno: isize,
        block_text: &str,
        b: Block,
    ) -> R<Result<Vec<Id>, DirectiveError>> {
        if b.content.is_empty() {
            return Ok(Err(DirectiveError(
                3,
                format!("Content block expected for the \"{name}\" directive; none found."),
            )));
        }
        let joined = b.content.data.join("\n");
        let mut out = Vec::new();
        for block in joined.split("\n\n") {
            if block.is_empty() {
                continue;
            }
            let node = self.doc.text_elem(Tag::MathBlock, block_text, block);
            let classes: Vec<(String, Option<String>)> = b
                .options
                .iter()
                .filter(|(n, _)| n == "class")
                .cloned()
                .collect();
            self.apply_options(node, &classes)?;
            self.doc.nm(node).line = Some((b.content_offset + 1) as usize);
            let names: Vec<(String, Option<String>)> = b
                .options
                .iter()
                .filter(|(n, _)| n == "name")
                .cloned()
                .collect();
            self.apply_options(node, &names)?;
            out.push(node);
        }
        Ok(Ok(out))
    }
}
