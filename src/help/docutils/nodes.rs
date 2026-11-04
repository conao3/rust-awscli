//! The document tree (`docutils/nodes.py`): an arena of nodes with the
//! `Element` attribute dictionary, `document`'s target bookkeeping
//! (`set_id`, `note_*_target`, ...) and the helpers the parser,
//! transforms and writer use.

use std::collections::{BTreeMap, HashMap};

use unicode_normalization::UnicodeNormalization;

use super::chars::split_ws;

pub(super) type Id = usize;

/// The node classes of `docutils.nodes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tag {
    Text,
    Element,
    Document,
    Section,
    Title,
    Subtitle,
    Meta,
    Transition,
    Paragraph,
    BulletList,
    EnumeratedList,
    ListItem,
    DefinitionList,
    DefinitionListItem,
    Term,
    Classifier,
    Definition,
    FieldList,
    Field,
    FieldName,
    FieldBody,
    Option,
    OptionArgument,
    OptionGroup,
    OptionList,
    OptionListItem,
    OptionString,
    Description,
    LiteralBlock,
    DoctestBlock,
    MathBlock,
    LineBlock,
    Line,
    BlockQuote,
    Attribution,
    Attention,
    Caution,
    Danger,
    Error,
    Important,
    Note,
    Tip,
    Hint,
    Warning,
    Admonition,
    Comment,
    SubstitutionDefinition,
    Target,
    Footnote,
    Citation,
    Label,
    Table,
    Tgroup,
    Colspec,
    Thead,
    Tbody,
    Row,
    Entry,
    SystemMessage,
    Emphasis,
    Strong,
    Literal,
    Reference,
    FootnoteReference,
    CitationReference,
    SubstitutionReference,
    TitleReference,
    Abbreviation,
    Acronym,
    Superscript,
    Subscript,
    Math,
    Problematic,
}

impl Tag {
    /// `isinstance(node, nodes.TextElement)`.
    pub(super) fn is_text_element(self) -> bool {
        use Tag::*;
        matches!(
            self,
            Title
                | Subtitle
                | Paragraph
                | Term
                | Classifier
                | FieldName
                | OptionArgument
                | OptionString
                | LiteralBlock
                | DoctestBlock
                | MathBlock
                | Line
                | Attribution
                | Comment
                | SubstitutionDefinition
                | Target
                | Label
                | Emphasis
                | Strong
                | Literal
                | Reference
                | FootnoteReference
                | CitationReference
                | SubstitutionReference
                | TitleReference
                | Abbreviation
                | Acronym
                | Superscript
                | Subscript
                | Math
                | Problematic
        )
    }
    /// `isinstance(node, nodes.PreBibliographic)`.
    pub(super) fn is_prebibliographic(self) -> bool {
        use Tag::*;
        matches!(
            self,
            Title | Subtitle | Meta | SystemMessage | Comment | SubstitutionDefinition | Target
        )
    }
    /// `isinstance(node, nodes.Invisible)`.
    pub(super) fn is_invisible(self) -> bool {
        matches!(
            self,
            Tag::Comment | Tag::SubstitutionDefinition | Tag::Target
        )
    }
    /// `isinstance(node, nodes.Inline)`.
    pub(super) fn is_inline(self) -> bool {
        use Tag::*;
        matches!(
            self,
            Target
                | Emphasis
                | Strong
                | Literal
                | Reference
                | FootnoteReference
                | CitationReference
                | SubstitutionReference
                | TitleReference
                | Abbreviation
                | Acronym
                | Superscript
                | Subscript
                | Math
                | Problematic
        )
    }
    /// `isinstance(node, nodes.Referential)`.
    pub(super) fn is_referential(self) -> bool {
        matches!(
            self,
            Tag::Reference | Tag::FootnoteReference | Tag::CitationReference
        )
    }
    /// `isinstance(node, nodes.Targetable)`.
    pub(super) fn is_targetable(self) -> bool {
        matches!(self, Tag::Target | Tag::Footnote | Tag::Citation)
    }
    /// `isinstance(node, nodes.Titular)`.
    pub(super) fn is_titular(self) -> bool {
        matches!(self, Tag::Title | Tag::Subtitle)
    }
    /// `node.tagname`.
    pub(super) fn name(self) -> &'static str {
        use Tag::*;
        match self {
            Text => "#text",
            Element => "Element",
            Document => "document",
            Section => "section",
            Title => "title",
            Subtitle => "subtitle",
            Meta => "meta",
            Transition => "transition",
            Paragraph => "paragraph",
            BulletList => "bullet_list",
            EnumeratedList => "enumerated_list",
            ListItem => "list_item",
            DefinitionList => "definition_list",
            DefinitionListItem => "definition_list_item",
            Term => "term",
            Classifier => "classifier",
            Definition => "definition",
            FieldList => "field_list",
            Field => "field",
            FieldName => "field_name",
            FieldBody => "field_body",
            Option => "option",
            OptionArgument => "option_argument",
            OptionGroup => "option_group",
            OptionList => "option_list",
            OptionListItem => "option_list_item",
            OptionString => "option_string",
            Description => "description",
            LiteralBlock => "literal_block",
            DoctestBlock => "doctest_block",
            MathBlock => "math_block",
            LineBlock => "line_block",
            Line => "line",
            BlockQuote => "block_quote",
            Attribution => "attribution",
            Attention => "attention",
            Caution => "caution",
            Danger => "danger",
            Error => "error",
            Important => "important",
            Note => "note",
            Tip => "tip",
            Hint => "hint",
            Warning => "warning",
            Admonition => "admonition",
            Comment => "comment",
            SubstitutionDefinition => "substitution_definition",
            Target => "target",
            Footnote => "footnote",
            Citation => "citation",
            Label => "label",
            Table => "table",
            Tgroup => "tgroup",
            Colspec => "colspec",
            Thead => "thead",
            Tbody => "tbody",
            Row => "row",
            Entry => "entry",
            SystemMessage => "system_message",
            Emphasis => "emphasis",
            Strong => "strong",
            Literal => "literal",
            Reference => "reference",
            FootnoteReference => "footnote_reference",
            CitationReference => "citation_reference",
            SubstitutionReference => "substitution_reference",
            TitleReference => "title_reference",
            Abbreviation => "abbreviation",
            Acronym => "acronym",
            Superscript => "superscript",
            Subscript => "subscript",
            Math => "math",
            Problematic => "problematic",
        }
    }
}

/// An attribute value.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Val {
    Str(String),
    Int(i64),
    List(Vec<String>),
}

impl Val {
    pub(super) fn as_str(&self) -> String {
        match self {
            Val::Str(s) => s.clone(),
            Val::Int(i) => i.to_string(),
            Val::List(l) => format!("{l:?}"),
        }
    }
    pub(super) fn truthy(&self) -> bool {
        match self {
            Val::Str(s) => !s.is_empty(),
            Val::Int(i) => *i != 0,
            Val::List(l) => !l.is_empty(),
        }
    }
}

const LIST_ATTRS: [&str; 5] = ["ids", "classes", "names", "dupnames", "backrefs"];

/// One node.
#[derive(Debug, Clone)]
pub(super) struct NodeData {
    pub tag: Tag,
    pub text: String,
    pub rawsource: String,
    pub attrs: BTreeMap<String, Val>,
    pub children: Vec<Id>,
    pub parent: Option<Id>,
    pub source: Option<String>,
    pub line: Option<usize>,
    pub indent: Option<usize>,
    pub resolved: bool,
    pub multiply_indirect: bool,
}

/// The `document` node and its arena.
pub(super) struct Doc {
    pub nodes: Vec<NodeData>,
    pub current_source: Option<String>,
    pub current_line: Option<usize>,
    pub ids: HashMap<String, Id>,
    pub nameids: HashMap<String, Option<String>>,
    pub nametypes: HashMap<String, bool>,
    pub refnames: HashMap<String, Vec<Id>>,
    pub refids: HashMap<String, Vec<Id>>,
    pub indirect_targets: Vec<Id>,
    pub substitution_defs: HashMap<String, Id>,
    pub substitution_names: HashMap<String, String>,
    pub footnote_refs: HashMap<String, Vec<Id>>,
    pub citation_refs: HashMap<String, Vec<Id>>,
    pub autofootnotes: Vec<Id>,
    pub autofootnote_refs: Vec<Id>,
    pub symbol_footnotes: Vec<Id>,
    pub symbol_footnote_refs: Vec<Id>,
    pub footnotes: Vec<Id>,
    pub citations: Vec<Id>,
    pub autofootnote_start: usize,
    pub symbol_footnote_start: usize,
    pub id_counter: HashMap<String, usize>,
    pub transform_messages: Vec<Id>,
    pub observe_transform: bool,
}

/// `nodes.unescape`.
pub(super) fn unescape(text: &str, restore_backslashes: bool) -> String {
    if restore_backslashes {
        text.replace('\x00', "\\")
    } else {
        let mut t = text.to_string();
        for sep in ["\x00 ", "\x00\n", "\x00"] {
            t = t.split(sep).collect::<Vec<_>>().join("");
        }
        t
    }
}

/// `nodes.whitespace_normalize_name`.
pub(super) fn whitespace_normalize_name(name: &str) -> String {
    split_ws(name).join(" ")
}

/// `nodes.fully_normalize_name`.
pub(super) fn fully_normalize_name(name: &str) -> String {
    split_ws(&name.to_lowercase()).join(" ")
}

fn translate_id_char(c: char) -> Option<&'static str> {
    Some(match c as u32 {
        0x00f8 => "o",
        0x0111 => "d",
        0x0127 => "h",
        0x0131 => "i",
        0x0142 => "l",
        0x0167 => "t",
        0x0180 => "b",
        0x0183 => "b",
        0x0188 => "c",
        0x018c => "d",
        0x0192 => "f",
        0x0199 => "k",
        0x019a => "l",
        0x019e => "n",
        0x01a5 => "p",
        0x01ab => "t",
        0x01ad => "t",
        0x01b4 => "y",
        0x01b6 => "z",
        0x01e5 => "g",
        0x0225 => "z",
        0x0234 => "l",
        0x0235 => "n",
        0x0236 => "t",
        0x0237 => "j",
        0x023c => "c",
        0x023f => "s",
        0x0240 => "z",
        0x0247 => "e",
        0x0249 => "j",
        0x024b => "q",
        0x024d => "r",
        0x024f => "y",
        0x00df => "sz",
        0x00e6 => "ae",
        0x0153 => "oe",
        0x0238 => "db",
        0x0239 => "qp",
        _ => return None,
    })
}

/// `nodes.make_id`.
pub(super) fn make_id(string: &str) -> String {
    let lower = string.to_lowercase();
    let mut t = String::new();
    for c in lower.chars() {
        match translate_id_char(c) {
            Some(r) => t.push_str(r),
            None => t.push(c),
        }
    }
    let ascii: String = t.nfkd().filter(char::is_ascii).collect();
    let joined = split_ws(&ascii).join(" ");
    let mut id = String::new();
    let mut in_bad = false;
    for c in joined.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            id.push(c);
            in_bad = false;
        } else if !in_bad {
            id.push('-');
            in_bad = true;
        }
    }
    let id = id.trim_start_matches(|c: char| c == '-' || c.is_ascii_digit());
    id.trim_end_matches('-').to_string()
}

impl Doc {
    pub(super) fn new() -> Doc {
        let mut d = Doc {
            nodes: Vec::new(),
            current_source: None,
            current_line: None,
            ids: HashMap::new(),
            nameids: HashMap::new(),
            nametypes: HashMap::new(),
            refnames: HashMap::new(),
            refids: HashMap::new(),
            indirect_targets: Vec::new(),
            substitution_defs: HashMap::new(),
            substitution_names: HashMap::new(),
            footnote_refs: HashMap::new(),
            citation_refs: HashMap::new(),
            autofootnotes: Vec::new(),
            autofootnote_refs: Vec::new(),
            symbol_footnotes: Vec::new(),
            symbol_footnote_refs: Vec::new(),
            footnotes: Vec::new(),
            citations: Vec::new(),
            autofootnote_start: 1,
            symbol_footnote_start: 0,
            id_counter: HashMap::new(),
            transform_messages: Vec::new(),
            observe_transform: false,
        };
        let root = d.elem(Tag::Document, "");
        d.set(root, "source", Val::Str("<string>".into()));
        d
    }

    pub(super) fn n(&self, id: Id) -> &NodeData {
        &self.nodes[id]
    }
    pub(super) fn nm(&mut self, id: Id) -> &mut NodeData {
        &mut self.nodes[id]
    }
    pub(super) fn tag(&self, id: Id) -> Tag {
        self.nodes[id].tag
    }

    /// `nodes.<tag>(rawsource)`.
    pub(super) fn elem(&mut self, tag: Tag, rawsource: &str) -> Id {
        let mut attrs = BTreeMap::new();
        for a in LIST_ATTRS {
            attrs.insert(a.to_string(), Val::List(Vec::new()));
        }
        if matches!(
            tag,
            Tag::LiteralBlock | Tag::DoctestBlock | Tag::MathBlock | Tag::Comment
        ) {
            attrs.insert("xml:space".into(), Val::Str("preserve".into()));
        }
        self.nodes.push(NodeData {
            tag,
            text: String::new(),
            rawsource: rawsource.to_string(),
            attrs,
            children: Vec::new(),
            parent: None,
            source: None,
            line: None,
            indent: None,
            resolved: false,
            multiply_indirect: false,
        });
        self.nodes.len() - 1
    }

    /// `nodes.Text(data)`.
    pub(super) fn text(&mut self, data: &str) -> Id {
        self.nodes.push(NodeData {
            tag: Tag::Text,
            text: data.to_string(),
            rawsource: String::new(),
            attrs: BTreeMap::new(),
            children: Vec::new(),
            parent: None,
            source: None,
            line: None,
            indent: None,
            resolved: false,
            multiply_indirect: false,
        });
        self.nodes.len() - 1
    }

    /// `TextElement(rawsource, text)`: an element with one `Text` child
    /// when `text` is not empty.
    pub(super) fn text_elem(&mut self, tag: Tag, rawsource: &str, text: &str) -> Id {
        let e = self.elem(tag, rawsource);
        if !text.is_empty() {
            let t = self.text(text);
            self.append(e, t);
        }
        e
    }

    pub(super) fn get(&self, id: Id, key: &str) -> Option<&Val> {
        self.nodes[id].attrs.get(key)
    }
    pub(super) fn has(&self, id: Id, key: &str) -> bool {
        self.nodes[id].attrs.contains_key(key)
    }
    pub(super) fn get_str(&self, id: Id, key: &str) -> Option<String> {
        self.get(id, key).map(Val::as_str)
    }
    pub(super) fn truthy(&self, id: Id, key: &str) -> bool {
        self.get(id, key).is_some_and(Val::truthy)
    }
    pub(super) fn set(&mut self, id: Id, key: &str, v: Val) {
        self.nodes[id].attrs.insert(key.to_string(), v);
    }
    pub(super) fn set_str(&mut self, id: Id, key: &str, v: &str) {
        self.set(id, key, Val::Str(v.to_string()));
    }
    pub(super) fn del(&mut self, id: Id, key: &str) {
        self.nodes[id].attrs.remove(key);
    }
    pub(super) fn list(&self, id: Id, key: &str) -> Vec<String> {
        match self.get(id, key) {
            Some(Val::List(l)) => l.clone(),
            _ => Vec::new(),
        }
    }
    pub(super) fn list_mut(&mut self, id: Id, key: &str) -> &mut Vec<String> {
        let v = self.nodes[id]
            .attrs
            .entry(key.to_string())
            .or_insert_with(|| Val::List(Vec::new()));
        if !matches!(v, Val::List(_)) {
            *v = Val::List(Vec::new());
        }
        match v {
            Val::List(l) => l,
            _ => unreachable!(),
        }
    }
    pub(super) fn push_list(&mut self, id: Id, key: &str, v: &str) {
        self.list_mut(id, key).push(v.to_string());
    }

    pub(super) fn children(&self, id: Id) -> &[Id] {
        &self.nodes[id].children
    }
    pub(super) fn parent(&self, id: Id) -> Option<Id> {
        self.nodes[id].parent
    }

    /// `node.document` is not `None`.
    fn attached(&self, mut id: Id) -> bool {
        loop {
            if id == 0 {
                return true;
            }
            match self.nodes[id].parent {
                Some(p) => id = p,
                None => return false,
            }
        }
    }

    /// `Node.setup_child`.
    fn setup_child(&mut self, parent: Id, child: Id) {
        self.nodes[child].parent = Some(parent);
        if self.attached(parent) {
            if self.nodes[child].source.is_none() {
                self.nodes[child].source = self.current_source.clone();
            }
            if self.nodes[child].line.is_none() {
                self.nodes[child].line = self.current_line;
            }
        }
    }

    /// `Element.append`.
    pub(super) fn append(&mut self, parent: Id, child: Id) {
        self.setup_child(parent, child);
        self.nodes[parent].children.push(child);
    }

    /// `Element.extend`.
    pub(super) fn extend(&mut self, parent: Id, children: &[Id]) {
        for &c in children {
            self.append(parent, c);
        }
    }

    /// `Element.insert`.
    pub(super) fn insert(&mut self, parent: Id, index: usize, child: Id) {
        self.setup_child(parent, child);
        self.nodes[parent].children.insert(index, child);
    }

    /// `node[index:index] = items`.
    pub(super) fn insert_many(&mut self, parent: Id, index: usize, items: &[Id]) {
        for &c in items {
            self.setup_child(parent, c);
        }
        let ch = &mut self.nodes[parent].children;
        let tail = ch.split_off(index.min(ch.len()));
        ch.extend_from_slice(items);
        ch.extend(tail);
    }

    /// `node[:] = items`.
    pub(super) fn set_children(&mut self, parent: Id, items: Vec<Id>) {
        for &c in &items {
            self.setup_child(parent, c);
        }
        self.nodes[parent].children = items;
    }

    /// `Element.index` (identity; `Text` compares by value).
    pub(super) fn index(&self, parent: Id, child: Id) -> usize {
        let ch = &self.nodes[parent].children;
        if self.nodes[child].tag == Tag::Text {
            let t = &self.nodes[child].text;
            ch.iter()
                .position(|&c| self.nodes[c].tag == Tag::Text && self.nodes[c].text == *t)
                .expect("child")
        } else {
            ch.iter().position(|&c| c == child).expect("child")
        }
    }

    /// `Element.remove`.
    pub(super) fn remove(&mut self, parent: Id, child: Id) {
        let i = self.index(parent, child);
        self.nodes[parent].children.remove(i);
    }

    /// `Element.replace(old, new)` with a list of new nodes.
    pub(super) fn replace(&mut self, parent: Id, old: Id, new: &[Id]) {
        let i = self.index(parent, old);
        for &c in new {
            self.setup_child(parent, c);
        }
        self.nodes[parent]
            .children
            .splice(i..i + 1, new.iter().copied());
    }

    /// `Element.replace_self(new)`.
    pub(super) fn replace_self(&mut self, old: Id, new: &[Id]) {
        if let Some(&first) = new.first()
            && self.nodes[first].tag != Tag::Text
        {
            for att in ["ids", "classes", "names", "dupnames"] {
                for v in self.list(old, att) {
                    if !self.list(first, att).contains(&v) {
                        self.push_list(first, att, &v);
                    }
                }
            }
        }
        let parent = self.nodes[old].parent.expect("parent");
        self.replace(parent, old, new);
    }

    /// `node.astext()`.
    pub(super) fn astext(&self, id: Id) -> String {
        let n = &self.nodes[id];
        match n.tag {
            Tag::Text => unescape(&n.text, false),
            Tag::SystemMessage => {
                let line = self.get_str(id, "line").unwrap_or_default();
                format!(
                    "{}:{}: ({}/{}) {}",
                    self.get_str(id, "source").unwrap_or_default(),
                    line,
                    self.get_str(id, "type").unwrap_or_default(),
                    self.get_str(id, "level").unwrap_or_default(),
                    self.element_astext(id)
                )
            }
            _ => self.element_astext(id),
        }
    }

    fn element_astext(&self, id: Id) -> String {
        let sep = if self.nodes[id].tag.is_text_element() {
            ""
        } else {
            "\n\n"
        };
        self.nodes[id]
            .children
            .iter()
            .map(|&c| self.astext(c))
            .collect::<Vec<_>>()
            .join(sep)
    }

    /// `node.findall(cls)` / `traverse` in document order (self included).
    pub(super) fn findall(&self, root: Id, pred: &dyn Fn(Tag) -> bool) -> Vec<Id> {
        let mut out = Vec::new();
        self.findall_into(root, pred, &mut out);
        out
    }

    fn findall_into(&self, id: Id, pred: &dyn Fn(Tag) -> bool, out: &mut Vec<Id>) {
        if pred(self.nodes[id].tag) {
            out.push(id);
        }
        for &c in &self.nodes[id].children {
            self.findall_into(c, pred, out);
        }
    }

    /// `node.next_node(ascend=True)` (descend=True) skipping `self`.
    pub(super) fn next_node_ascend(&self, id: Id, descend: bool) -> Option<Id> {
        if descend && let Some(&c) = self.nodes[id].children.first() {
            return Some(c);
        }
        let mut node = id;
        while let Some(p) = self.nodes[node].parent {
            let ch = &self.nodes[p].children;
            let i = ch.iter().position(|&c| c == node).expect("child");
            if let Some(&s) = ch.get(i + 1) {
                return Some(s);
            }
            node = p;
        }
        None
    }

    /// `Element.first_child_not_matching_class`.
    pub(super) fn first_child_not_matching(
        &self,
        id: Id,
        pred: &dyn Fn(Tag) -> bool,
    ) -> Option<usize> {
        self.nodes[id]
            .children
            .iter()
            .position(|&c| !pred(self.nodes[c].tag))
    }

    /// `node.deepcopy()`.
    pub(super) fn deepcopy(&mut self, id: Id) -> Id {
        let mut data = self.nodes[id].clone();
        data.parent = None;
        let children = std::mem::take(&mut data.children);
        self.nodes.push(data);
        let new = self.nodes.len() - 1;
        for c in children {
            let cc = self.deepcopy(c);
            self.nodes[cc].parent = Some(new);
            self.nodes[new].children.push(cc);
        }
        new
    }

    /// `utils.get_source_line`.
    pub(super) fn source_line(&self, mut id: Id) -> (Option<String>, Option<usize>) {
        loop {
            let n = &self.nodes[id];
            if n.source.as_deref().is_some_and(|s| !s.is_empty()) || n.line.is_some_and(|l| l != 0)
            {
                return (n.source.clone(), n.line);
            }
            match n.parent {
                Some(p) => id = p,
                None => return (None, None),
            }
        }
    }

    /// `document.set_id`.
    pub(super) fn set_id(&mut self, node: Id) -> String {
        let ids = self.list(node, "ids");
        if !ids.is_empty() {
            for id in &ids {
                self.ids.entry(id.clone()).or_insert(node);
            }
            return ids.last().unwrap().clone();
        }
        let mut base_id = String::new();
        let mut id = String::new();
        let mut found = false;
        for name in self.list(node, "names") {
            base_id = make_id(&name);
            id = base_id.clone();
            if !base_id.is_empty() && !self.ids.contains_key(&id) {
                found = true;
                break;
            }
        }
        if !found {
            let prefix = if !base_id.is_empty() {
                format!("{id}-")
            } else {
                format!("{}-", make_id(self.nodes[node].tag.name()))
            };
            loop {
                let c = self.id_counter.entry(prefix.clone()).or_insert(0);
                *c += 1;
                id = format!("{prefix}{c}");
                if !self.ids.contains_key(&id) {
                    break;
                }
            }
        }
        self.push_list(node, "ids", &id);
        self.ids.insert(id.clone(), node);
        id
    }

    /// `nodes.dupname`.
    pub(super) fn dupname(&mut self, node: Id, name: &str) {
        self.push_list(node, "dupnames", name);
        let names = self.list_mut(node, "names");
        if let Some(i) = names.iter().position(|n| n == name) {
            names.remove(i);
        }
    }

    pub(super) fn note_refname(&mut self, node: Id) {
        let name = self.get_str(node, "refname").unwrap_or_default();
        self.refnames.entry(name).or_default().push(node);
    }

    pub(super) fn note_refid(&mut self, node: Id) {
        let id = self.get_str(node, "refid").unwrap_or_default();
        self.refids.entry(id).or_default().push(node);
    }

    pub(super) fn note_indirect_target(&mut self, target: Id) {
        self.indirect_targets.push(target);
        if !self.list(target, "names").is_empty() {
            self.note_refname(target);
        }
    }

    pub(super) fn note_anonymous_target(&mut self, target: Id) {
        self.set_id(target);
    }

    pub(super) fn note_substitution_ref(&mut self, subref: Id, refname: &str) {
        self.set_str(subref, "refname", &whitespace_normalize_name(refname));
    }

    pub(super) fn add_backref(&mut self, node: Id, refid: &str) {
        self.push_list(node, "backrefs", refid);
    }
}
