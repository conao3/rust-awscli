//! `awscli/bcdoc/restdoc.py` `ReSTDocument`, `awscli/bcdoc/style.py`
//! `ReSTStyle`, and `awscli/bcdoc/docstringparser.py` (the model's HTML
//! documentation to reStructuredText).

use indexmap::IndexMap;

use super::html::{Handler, HtmlParser};

/// An exception raised while writing (`IndexError` from `pop_write` on an
/// empty stack, `AttributeError` from a closed HTML tree).
#[derive(Debug)]
pub(super) struct Raised;

type Res = Result<(), Raised>;

/// Python's `str.isspace` for one character.
pub(super) fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// `ReSTDocument` with its `ReSTStyle` (target `man`).
pub(crate) struct ReSTDocument {
    writes: Vec<String>,
    indent: usize,
    do_p: bool,
    a_href: Option<String>,
    list_depth: i64,
    keep_data: bool,
    pub(super) hrefs: IndexMap<String, String>,
    parser: DocStringParser,
}

impl Default for ReSTDocument {
    fn default() -> Self {
        ReSTDocument::new()
    }
}

impl ReSTDocument {
    pub(crate) fn new() -> ReSTDocument {
        ReSTDocument {
            writes: Vec::new(),
            indent: 0,
            do_p: true,
            a_href: None,
            list_depth: 0,
            keep_data: true,
            hrefs: IndexMap::new(),
            parser: DocStringParser::new(),
        }
    }

    fn write_raw(&mut self, s: &str) {
        if self.keep_data {
            self.writes.push(s.to_string());
        }
    }

    /// `ReSTDocument.write`.
    pub(crate) fn write(&mut self, s: &str) {
        self.write_raw(s);
    }

    /// `ReSTDocument.writeln`.
    pub(crate) fn writeln(&mut self, s: &str) {
        let line = format!("{}{s}\n", self.spaces());
        self.write_raw(&line);
    }

    fn pop_write(&mut self) -> Result<String, Raised> {
        self.writes.pop().ok_or(Raised)
    }

    fn push_write(&mut self, s: String) {
        self.writes.push(s);
    }

    /// `ReSTDocument.pop_write` (`None` where Python raises `IndexError`).
    pub(crate) fn pop_write_opt(&mut self) -> Option<String> {
        self.writes.pop()
    }

    /// `ReSTDocument.find_last_write`.
    pub(crate) fn find_last_write(&self, content: &str) -> Option<usize> {
        self.writes.iter().rposition(|w| w == content)
    }

    /// `ReSTDocument.insert_write`.
    pub(crate) fn insert_write(&mut self, index: usize, content: String) {
        let index = index.min(self.writes.len());
        self.writes.insert(index, content);
    }

    /// `style.indentation`.
    pub(crate) fn indentation(&self) -> usize {
        self.indent
    }

    /// `ReSTDocument.getvalue`.
    pub(crate) fn getvalue(&mut self) -> String {
        if !self.hrefs.is_empty() {
            self.new_paragraph();
            for (refname, link) in self.hrefs.clone() {
                self.link_target_definition(&refname, &link);
            }
        }
        self.writes.concat()
    }

    /// `ReSTDocument.handle_data`.
    fn handle_data(&mut self, data: &str) {
        if !data.is_empty() && self.keep_data {
            self.writes.push(data.to_string());
        }
    }

    /// `ReSTDocument.include_doc_string`.
    pub(crate) fn include_doc_string(&mut self, doc_string: &str) {
        if doc_string.is_empty() {
            return;
        }
        let mut parser = std::mem::take(&mut self.parser);
        let _ = parser
            .feed(doc_string, self)
            .and_then(|()| parser.close(self));
        self.parser = parser;
    }

    /// `ReSTDocument.write_from_file`: each line stripped and written with
    /// `writeln`.
    pub(crate) fn write_from_file(&mut self, contents: &str) {
        for line in py_readlines(contents) {
            self.writeln(line.trim_matches(py_isspace));
        }
    }

    pub(crate) fn spaces(&self) -> String {
        " ".repeat(self.indent * 2)
    }

    pub(crate) fn indent(&mut self) {
        self.indent += 1;
    }

    pub(crate) fn dedent(&mut self) {
        if self.indent > 0 {
            self.indent -= 1;
        }
    }

    pub(crate) fn new_paragraph(&mut self) {
        let s = format!("\n\n{}", self.spaces());
        self.write(&s);
    }

    pub(crate) fn new_line(&mut self) {
        let s = format!("\n{}", self.spaces());
        self.write(&s);
    }

    fn end_inline(&mut self, markup: &str) -> Res {
        let last_write = self.pop_write()?.trim_end_matches(' ').to_string();
        if last_write == markup {
            return Ok(());
        }
        self.push_write(last_write);
        self.write(&format!("{markup} "));
        Ok(())
    }

    fn heading(&mut self, s: &str, border_char: char) {
        let border: String = std::iter::repeat_n(border_char, s.chars().count()).collect();
        self.new_paragraph();
        self.write(&format!("{border}\n{s}\n{border}"));
        self.new_paragraph();
    }

    pub(crate) fn h1(&mut self, s: &str) {
        self.heading(s, '*');
    }

    pub(crate) fn h2(&mut self, s: &str) {
        self.heading(s, '=');
    }

    pub(crate) fn h3(&mut self, s: &str) {
        self.heading(s, '-');
    }

    pub(crate) fn start_note(&mut self) {
        self.new_paragraph();
        self.write(".. note::");
        self.indent();
        self.new_paragraph();
    }

    pub(crate) fn end_note(&mut self) {
        self.dedent();
        self.new_paragraph();
    }

    pub(crate) fn start_li(&mut self) {
        self.new_line();
        self.do_p = false;
        self.write("* ");
    }

    pub(crate) fn end_li(&mut self) {
        self.do_p = true;
        self.new_line();
    }

    /// `ReSTStyle.li`.
    pub(crate) fn li(&mut self, s: &str) {
        if !s.is_empty() {
            self.start_li();
            self.writeln(s);
            self.end_li();
        }
    }

    pub(crate) fn start_ul(&mut self) {
        if self.list_depth != 0 {
            self.indent();
        }
        self.list_depth += 1;
        self.new_paragraph();
    }

    pub(crate) fn end_ul(&mut self) {
        self.list_depth -= 1;
        if self.list_depth != 0 {
            self.dedent();
        }
        self.new_paragraph();
    }

    pub(crate) fn start_codeblock(&mut self) {
        self.write("::");
        self.indent();
        self.new_paragraph();
    }

    pub(crate) fn end_codeblock(&mut self) {
        self.dedent();
        self.new_paragraph();
    }

    /// `ReSTStyle.codeblock`.
    pub(crate) fn codeblock(&mut self, code: &str) {
        self.start_codeblock();
        self.writeln(code);
        self.end_codeblock();
    }

    /// `ReSTStyle.toctree` for the `man` target.
    pub(crate) fn toctree(&mut self) {
        self.start_ul();
    }

    /// `ReSTStyle.tocitem` for the `man` target.
    pub(crate) fn tocitem(&mut self, item: &str) {
        self.li(item);
    }

    pub(crate) fn link_target_definition(&mut self, refname: &str, link: &str) {
        self.writeln(&format!(".. _{refname}: {link}"));
    }

    /// `ReSTStyle.sphinx_reference_label` for the `man` target.
    pub(crate) fn sphinx_reference_label(&mut self, text: &str) {
        self.write(text);
    }

    /// `ReSTStyle.external_link` for the `man` target.
    pub(crate) fn external_link(&mut self, title: &str) {
        self.write(title);
    }

    fn start_a(&mut self, attrs: &[(String, Option<String>)]) {
        if attrs.is_empty() {
            self.write(" ");
        } else {
            for (k, v) in attrs {
                if k == "href" {
                    self.a_href = v.clone();
                    self.write("`");
                }
            }
        }
    }

    fn end_a(&mut self) -> Res {
        if let Some(href) = self.a_href.clone().filter(|h| !h.is_empty()) {
            let last_write = self.pop_write()?.trim_end_matches(' ').to_string();
            if !last_write.is_empty() && last_write != "`" {
                let last_write = if last_write.contains(':') {
                    last_write.replace(':', "\\:")
                } else {
                    last_write
                };
                self.push_write(last_write);
                self.push_write(format!(" <{href}>`__"));
            } else if last_write == "`" {
                self.push_write(format!("`<{href}>`__"));
            } else {
                self.push_write(href.clone());
                self.hrefs.insert(href.clone(), href);
                self.write("`__");
            }
            self.a_href = None;
        }
        self.write(" ");
        Ok(())
    }

    fn has_handler(tag: &str) -> bool {
        matches!(
            tag,
            "bold"
                | "b"
                | "italics"
                | "i"
                | "p"
                | "code"
                | "note"
                | "important"
                | "danger"
                | "a"
                | "li"
                | "ul"
                | "ol"
                | "examples"
                | "fullname"
                | "codeblock"
        )
    }

    fn start_tag(&mut self, tag: &str, attrs: &[(String, Option<String>)]) {
        match tag {
            "bold" | "b" => self.write("**"),
            "italics" | "i" => self.write("*"),
            "code" => self.write("``"),
            "p" => {
                if self.do_p {
                    self.new_paragraph();
                }
            }
            "note" => self.start_note(),
            "important" | "danger" => {
                self.new_paragraph();
                self.write(if tag == "important" {
                    ".. warning::"
                } else {
                    ".. danger::"
                });
                self.indent();
                self.new_paragraph();
            }
            "a" => self.start_a(attrs),
            "li" => self.start_li(),
            "ul" | "ol" => self.start_ul(),
            "examples" | "fullname" => self.keep_data = false,
            "codeblock" => self.start_codeblock(),
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: &str) -> Res {
        match tag {
            "bold" | "b" => self.end_inline("**")?,
            "italics" | "i" => self.end_inline("*")?,
            "code" => self.end_inline("``")?,
            "p" => {
                if self.do_p {
                    self.new_paragraph();
                }
            }
            "note" | "important" | "danger" => self.end_note(),
            "a" => self.end_a()?,
            "li" => self.end_li(),
            "ul" | "ol" => self.end_ul(),
            "examples" | "fullname" => self.keep_data = true,
            "codeblock" => self.end_codeblock(),
            _ => {}
        }
        Ok(())
    }
}

/// Python's `str.splitlines(keepends=True)` as `readlines()` sees text
/// read with universal newlines (only `\n` ends a line).
pub(super) fn py_readlines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

#[derive(Debug)]
enum NodeKind {
    Stem,
    Tag {
        tag: String,
        attrs: Vec<(String, Option<String>)>,
        line_item: bool,
    },
    Data(String),
}

#[derive(Debug)]
struct Node {
    parent: Option<usize>,
    kind: NodeKind,
    children: Vec<usize>,
}

/// `docstringparser.HTMLTree`.
#[derive(Debug)]
struct HtmlTree {
    nodes: Vec<Node>,
    current: Option<usize>,
}

impl HtmlTree {
    fn new() -> HtmlTree {
        HtmlTree {
            nodes: vec![Node {
                parent: None,
                kind: NodeKind::Stem,
                children: Vec::new(),
            }],
            current: Some(0),
        }
    }

    fn add_child(&mut self, kind: NodeKind) -> Result<usize, ()> {
        let parent = self.current.ok_or(())?;
        let id = self.nodes.len();
        self.nodes.push(Node {
            parent: Some(parent),
            kind,
            children: Vec::new(),
        });
        self.nodes[parent].children.push(id);
        Ok(id)
    }

    /// `LineItemNode._lstrip`.
    fn lstrip(&mut self, node: usize) -> bool {
        for child in self.nodes[node].children.clone() {
            if let NodeKind::Data(d) = &mut self.nodes[child].kind {
                *d = d.trim_start_matches(py_isspace).to_string();
                if !d.is_empty() {
                    return true;
                }
            } else if self.lstrip(child) {
                return true;
            }
        }
        false
    }

    fn write(&mut self, node: usize, doc: &mut ReSTDocument) -> Res {
        match &self.nodes[node].kind {
            NodeKind::Stem => self.write_children(node, doc),
            NodeKind::Tag { line_item, .. } => {
                if *line_item {
                    self.lstrip(node);
                }
                let NodeKind::Tag { tag, attrs, .. } = &self.nodes[node].kind else {
                    unreachable!()
                };
                let (tag, attrs) = (tag.clone(), attrs.clone());
                doc.start_tag(&tag, &attrs);
                self.write_children(node, doc)?;
                doc.end_tag(&tag)
            }
            NodeKind::Data(data) => {
                if data.is_empty() {
                    return Ok(());
                }
                let s = if data.chars().all(py_isspace) {
                    " ".to_string()
                } else {
                    let end_space = data.chars().last().is_some_and(py_isspace);
                    let mut s = data
                        .split(py_isspace)
                        .filter(|w| !w.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ");
                    if end_space {
                        s.push(' ');
                    }
                    s
                };
                doc.handle_data(&s);
                Ok(())
            }
        }
    }

    fn write_children(&mut self, node: usize, doc: &mut ReSTDocument) -> Res {
        for child in self.nodes[node].children.clone() {
            self.write(child, doc)?;
        }
        Ok(())
    }
}

impl Handler for HtmlTree {
    fn handle_starttag(&mut self, tag: &str, attrs: &[(String, Option<String>)]) -> Result<(), ()> {
        if !ReSTDocument::has_handler(tag) {
            return Ok(());
        }
        let id = self.add_child(NodeKind::Tag {
            tag: tag.to_string(),
            attrs: attrs.to_vec(),
            line_item: tag == "li",
        })?;
        self.current = Some(id);
        Ok(())
    }

    fn handle_endtag(&mut self, tag: &str) -> Result<(), ()> {
        if !ReSTDocument::has_handler(tag) {
            return Ok(());
        }
        let current = self.current.ok_or(())?;
        self.current = self.nodes[current].parent;
        Ok(())
    }

    fn handle_data(&mut self, data: &str) -> Result<(), ()> {
        self.add_child(NodeKind::Data(data.to_string())).map(|_| ())
    }
}

/// `docstringparser.DocStringParser`.
#[derive(Debug)]
struct DocStringParser {
    html: HtmlParser,
    tree: HtmlTree,
}

impl Default for DocStringParser {
    fn default() -> Self {
        DocStringParser::new()
    }
}

impl DocStringParser {
    fn new() -> DocStringParser {
        DocStringParser {
            html: HtmlParser::new(),
            tree: HtmlTree::new(),
        }
    }

    fn feed(&mut self, data: &str, doc: &mut ReSTDocument) -> Res {
        self.html.feed(data, &mut self.tree).map_err(|()| Raised)?;
        self.tree.write(0, doc)?;
        self.tree = HtmlTree::new();
        Ok(())
    }

    fn close(&mut self, doc: &mut ReSTDocument) -> Res {
        self.html.close(&mut self.tree).map_err(|()| Raised)?;
        self.tree.write(0, doc)?;
        self.tree = HtmlTree::new();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rst(html: &str) -> String {
        let mut doc = ReSTDocument::new();
        doc.include_doc_string(html);
        doc.getvalue()
    }

    #[test]
    fn converts_model_html() {
        assert_eq!(
            rst(
                "<p>Use <code>foo</code> with <a href=\"https://x\">the guide</a>.</p><ul><li><p>one</p></li></ul>"
            ),
            "\n\nUse ``foo`` with `the guide <https://x>`__ .\n\n\n\n\n* one\n\n\n"
        );
    }
}
