//! A backtracking matcher for the subset of Python `re` syntax the
//! reStructuredText parser uses (lookaround, named groups,
//! backreferences, `re.VERBOSE`), with `str`-pattern semantics for `\s`,
//! `\w`, `\d`, `^` and `$`.

use super::chars::{is_decimal, is_space, is_word};

#[derive(Debug, Clone)]
enum ClassItem {
    Range(char, char),
    Space(bool),
    Word(bool),
    Digit(bool),
}

#[derive(Debug, Clone)]
enum Node {
    Empty,
    Char(char),
    Any,
    Class(Box<CharClass>),
    Start,
    End,
    Group(Box<Node>, Option<usize>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, usize, Option<usize>, bool),
    Look(Box<Node>, bool, bool, usize),
    Backref(usize),
    Pred(Box<Node>, Box<Node>),
}

/// A compiled pattern (`re.compile`).
#[derive(Debug, Clone)]
pub(super) struct Regex {
    root: Node,
    ngroups: usize,
    names: Vec<(String, usize)>,
}

/// A match object: group spans in code points of the searched text.
#[derive(Debug, Clone)]
pub(super) struct Match {
    caps: Vec<Option<(usize, usize)>>,
    names: Vec<(String, usize)>,
    text: Vec<char>,
}

impl Match {
    pub(super) fn start(&self) -> usize {
        self.caps[0].unwrap().0
    }
    pub(super) fn end(&self) -> usize {
        self.caps[0].unwrap().1
    }
    fn idx(&self, name: &str) -> usize {
        self.names
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, i)| *i)
            .unwrap_or_else(|| panic!("no group {name}"))
    }
    /// `match.span(i)`.
    pub(super) fn span(&self, i: usize) -> Option<(usize, usize)> {
        self.caps[i]
    }
    /// `match.span(name)`.
    pub(super) fn span_n(&self, name: &str) -> Option<(usize, usize)> {
        self.caps[self.idx(name)]
    }
    /// `match.group(i)` (`None` for a group that did not participate).
    pub(super) fn group(&self, i: usize) -> Option<String> {
        self.caps[i].map(|(a, b)| self.text[a..b].iter().collect())
    }
    /// `match.group(name)`.
    pub(super) fn name(&self, name: &str) -> Option<String> {
        self.group(self.idx(name))
    }
    /// `match.group(name) or ''` is truthy.
    pub(super) fn has(&self, name: &str) -> bool {
        self.span_n(name).is_some_and(|(a, b)| b > a)
    }
    /// `match.string`.
    pub(super) fn string(&self) -> &[char] {
        &self.text
    }
}

struct Parser<'a> {
    p: &'a [char],
    i: usize,
    verbose: bool,
    ngroups: usize,
    names: Vec<(String, usize)>,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        if !self.verbose {
            return;
        }
        while self.i < self.p.len() {
            let c = self.p[self.i];
            if c.is_whitespace() {
                self.i += 1;
            } else if c == '#' {
                while self.i < self.p.len() && self.p[self.i] != '\n' {
                    self.i += 1;
                }
            } else {
                break;
            }
        }
    }
    fn peek(&mut self) -> Option<char> {
        self.skip_ws();
        self.p.get(self.i).copied()
    }
    fn alt(&mut self) -> Node {
        let mut alts = vec![self.concat()];
        while self.peek() == Some('|') {
            self.i += 1;
            alts.push(self.concat());
        }
        if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            Node::Alt(alts)
        }
    }
    fn concat(&mut self) -> Node {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom();
            let atom = self.quant(atom);
            items.push(atom);
        }
        match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap(),
            _ => Node::Concat(items),
        }
    }
    fn quant(&mut self, atom: Node) -> Node {
        let Some(c) = self.peek() else { return atom };
        let (min, max) = match c {
            '*' => (0, None),
            '+' => (1, None),
            '?' => (0, Some(1)),
            '{' => {
                let save = self.i;
                self.i += 1;
                let mut a = String::new();
                while self.p[self.i].is_ascii_digit() {
                    a.push(self.p[self.i]);
                    self.i += 1;
                }
                if self.p[self.i] == '}' {
                    let n: usize = a.parse().unwrap();
                    (n, Some(n))
                } else if self.p[self.i] == ',' {
                    self.i += 1;
                    let mut b = String::new();
                    while self.p[self.i].is_ascii_digit() {
                        b.push(self.p[self.i]);
                        self.i += 1;
                    }
                    assert_eq!(self.p[self.i], '}');
                    (
                        if a.is_empty() { 0 } else { a.parse().unwrap() },
                        if b.is_empty() {
                            None
                        } else {
                            Some(b.parse().unwrap())
                        },
                    )
                } else {
                    self.i = save;
                    return atom;
                }
            }
            _ => return atom,
        };
        self.i += 1;
        let greedy = if self.p.get(self.i) == Some(&'?') {
            self.i += 1;
            false
        } else {
            true
        };
        Node::Repeat(Box::new(atom), min, max, greedy)
    }
    fn escape(&mut self) -> Result<char, ClassItem> {
        let c = self.p[self.i];
        self.i += 1;
        match c {
            's' => Err(ClassItem::Space(true)),
            'S' => Err(ClassItem::Space(false)),
            'w' => Err(ClassItem::Word(true)),
            'W' => Err(ClassItem::Word(false)),
            'd' => Err(ClassItem::Digit(true)),
            'D' => Err(ClassItem::Digit(false)),
            'n' => Ok('\n'),
            't' => Ok('\t'),
            'x' => {
                let h: String = self.p[self.i..self.i + 2].iter().collect();
                self.i += 2;
                Ok(char::from_u32(u32::from_str_radix(&h, 16).unwrap()).unwrap())
            }
            'u' => {
                let h: String = self.p[self.i..self.i + 4].iter().collect();
                self.i += 4;
                Ok(char::from_u32(u32::from_str_radix(&h, 16).unwrap()).unwrap())
            }
            _ => Ok(c),
        }
    }
    fn class(&mut self) -> Node {
        let mut items = Vec::new();
        let negate = if self.p[self.i] == '^' {
            self.i += 1;
            true
        } else {
            false
        };
        let mut first = true;
        loop {
            let c = self.p[self.i];
            if c == ']' && !first {
                self.i += 1;
                break;
            }
            first = false;
            self.i += 1;
            let lo = if c == '\\' {
                match self.escape() {
                    Ok(ch) => ch,
                    Err(item) => {
                        items.push(item);
                        continue;
                    }
                }
            } else {
                c
            };
            if self.p[self.i] == '-' && self.p.get(self.i + 1).is_some_and(|&n| n != ']') {
                self.i += 1;
                let c2 = self.p[self.i];
                self.i += 1;
                let hi = if c2 == '\\' {
                    self.escape().unwrap_or('\\')
                } else {
                    c2
                };
                items.push(ClassItem::Range(lo, hi));
            } else {
                items.push(ClassItem::Range(lo, lo));
            }
        }
        Node::Class(Box::new(CharClass::new(items, negate)))
    }
    fn atom(&mut self) -> Node {
        let c = self.p[self.i];
        self.i += 1;
        match c {
            '.' => Node::Any,
            '^' => Node::Start,
            '$' => Node::End,
            '[' => self.class(),
            '\\' => {
                let d = self.p[self.i];
                if d.is_ascii_digit() && d != '0' {
                    self.i += 1;
                    return Node::Backref(d.to_digit(10).unwrap() as usize);
                }
                match self.escape() {
                    Ok(ch) => Node::Char(ch),
                    Err(item) => Node::Class(Box::new(CharClass::new(vec![item], false))),
                }
            }
            '(' => {
                let mut capture = true;
                let mut look = None;
                let mut name = None;
                if self.p[self.i] == '?' {
                    self.i += 1;
                    match self.p[self.i] {
                        ':' => {
                            self.i += 1;
                            capture = false;
                        }
                        '=' | '!' => {
                            look = Some((true, self.p[self.i] == '!'));
                            self.i += 1;
                        }
                        '<' => {
                            self.i += 1;
                            look = Some((false, self.p[self.i] == '!'));
                            self.i += 1;
                        }
                        'P' => {
                            self.i += 1;
                            if self.p[self.i] == '=' {
                                self.i += 1;
                                let mut n = String::new();
                                while self.p[self.i] != ')' {
                                    n.push(self.p[self.i]);
                                    self.i += 1;
                                }
                                self.i += 1;
                                let g = self.names.iter().find(|(x, _)| *x == n).unwrap().1;
                                return Node::Backref(g);
                            }
                            self.i += 1;
                            let mut n = String::new();
                            while self.p[self.i] != '>' {
                                n.push(self.p[self.i]);
                                self.i += 1;
                            }
                            self.i += 1;
                            name = Some(n);
                        }
                        other => panic!("unsupported group (?{other}"),
                    }
                }
                if let Some((ahead, negate)) = look {
                    let inner = self.alt();
                    assert_eq!(self.peek(), Some(')'));
                    self.i += 1;
                    let w = if ahead { 0 } else { width(&inner) };
                    return Node::Look(Box::new(inner), ahead, negate, w);
                }
                let idx = if capture {
                    self.ngroups += 1;
                    let g = self.ngroups;
                    if let Some(n) = name {
                        self.names.push((n, g));
                    }
                    Some(g)
                } else {
                    None
                };
                let inner = self.alt();
                assert_eq!(self.peek(), Some(')'), "unbalanced pattern");
                self.i += 1;
                if idx.is_none()
                    && let Node::Concat(v) = &inner
                    && v.len() == 2
                    && let Node::Look(look, true, true, _) = &v[0]
                    && single(look, ' ').is_some()
                    && single(&v[1], ' ').is_some()
                {
                    return Node::Pred(look.clone(), Box::new(v[1].clone()));
                }
                Node::Group(Box::new(inner), idx)
            }
            _ => Node::Char(c),
        }
    }
}

fn width(n: &Node) -> usize {
    match n {
        Node::Empty | Node::Start | Node::End | Node::Look(..) => 0,
        Node::Char(_) | Node::Any | Node::Class(..) => 1,
        Node::Group(inner, _) => width(inner),
        Node::Concat(v) => v.iter().map(width).sum(),
        Node::Alt(v) => width(&v[0]),
        Node::Repeat(inner, min, _, _) => width(inner) * min,
        Node::Backref(_) => panic!("lookbehind with a backreference"),
        Node::Pred(..) => 1,
    }
}

type Caps = Vec<Option<(usize, usize)>>;

struct Ctx<'t> {
    t: &'t [char],
}

/// A compiled character class: an ASCII bitmap and sorted ranges.
#[derive(Debug, Clone)]
struct CharClass {
    ascii: u128,
    ranges: Vec<(char, char)>,
    specials: Vec<ClassItem>,
    negate: bool,
}

impl CharClass {
    fn new(items: Vec<ClassItem>, negate: bool) -> CharClass {
        let mut ascii = 0u128;
        for c in 0u8..128 {
            if class_match(&items, false, c as char) {
                ascii |= 1 << c;
            }
        }
        let mut ranges: Vec<(char, char)> = items
            .iter()
            .filter_map(|i| match i {
                ClassItem::Range(a, b) => Some((*a, *b)),
                _ => None,
            })
            .collect();
        ranges.sort();
        let specials = items
            .into_iter()
            .filter(|i| !matches!(i, ClassItem::Range(..)))
            .collect();
        CharClass {
            ascii,
            ranges,
            specials,
            negate,
        }
    }

    fn matches(&self, c: char) -> bool {
        let hit = if (c as u32) < 128 {
            self.ascii & (1 << c as u32) != 0
        } else {
            let i = self.ranges.partition_point(|r| r.0 <= c);
            self.ranges[..i].iter().rev().any(|r| c <= r.1) || class_match(&self.specials, false, c)
        };
        hit != self.negate
    }
}

fn class_match(items: &[ClassItem], negate: bool, c: char) -> bool {
    let hit = items.iter().any(|it| match it {
        ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
        ClassItem::Space(p) => is_space(c) == *p,
        ClassItem::Word(p) => is_word(c) == *p,
        ClassItem::Digit(p) => is_decimal(c) == *p,
    });
    hit != negate
}

fn single(n: &Node, c: char) -> Option<bool> {
    match n {
        Node::Pred(not, yes) => Some(!single(not, c).unwrap() && single(yes, c).unwrap()),
        Node::Char(x) => Some(c == *x),
        Node::Any => Some(c != '\n'),
        Node::Class(cc) => Some(cc.matches(c)),
        _ => None,
    }
}

impl Ctx<'_> {
    fn m(
        &self,
        n: &Node,
        i: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(usize, &mut Caps) -> bool,
    ) -> bool {
        match n {
            Node::Empty => k(i, caps),
            Node::Char(_) | Node::Any | Node::Class(..) | Node::Pred(..) => {
                if i < self.t.len() && single(n, self.t[i]).unwrap() {
                    k(i + 1, caps)
                } else {
                    false
                }
            }
            Node::Start => i == 0 && k(i, caps),
            Node::End => {
                let len = self.t.len();
                (i == len || (i + 1 == len && self.t[i] == '\n')) && k(i, caps)
            }
            Node::Group(inner, idx) => match idx {
                None => self.m(inner, i, caps, k),
                Some(g) => {
                    let g = *g;
                    self.m(inner, i, caps, &mut |j, caps: &mut Caps| {
                        let old = caps[g];
                        caps[g] = Some((i, j));
                        if k(j, caps) {
                            true
                        } else {
                            caps[g] = old;
                            false
                        }
                    })
                }
            },
            Node::Concat(v) => self.seq(v, 0, i, caps, k),
            Node::Alt(v) => {
                for a in v {
                    if self.m(a, i, caps, k) {
                        return true;
                    }
                }
                false
            }
            Node::Repeat(inner, min, max, greedy) => {
                if single(inner, ' ').is_some() {
                    let limit = max.unwrap_or(usize::MAX);
                    let mut n = 0;
                    while n < limit && i + n < self.t.len() && single(inner, self.t[i + n]).unwrap()
                    {
                        n += 1;
                    }
                    if n < *min {
                        return false;
                    }
                    if *greedy {
                        for c in (*min..=n).rev() {
                            if k(i + c, caps) {
                                return true;
                            }
                        }
                    } else {
                        for c in *min..=n {
                            if k(i + c, caps) {
                                return true;
                            }
                        }
                    }
                    false
                } else {
                    self.rep(inner, *min, *max, *greedy, 0, i, caps, k)
                }
            }
            Node::Look(inner, ahead, negate, w) => {
                let start = if *ahead {
                    i
                } else if i < *w {
                    return *negate && k(i, caps);
                } else {
                    i - w
                };
                let saved = caps.clone();
                let ok = self.m(inner, start, caps, &mut |j, _| *ahead || j == i);
                if ok == *negate {
                    *caps = saved;
                    return false;
                }
                if *negate {
                    *caps = saved;
                }
                k(i, caps)
            }
            Node::Backref(g) => match caps[*g] {
                Some((a, b)) => {
                    let len = b - a;
                    if i + len <= self.t.len() && self.t[a..b] == self.t[i..i + len] {
                        k(i + len, caps)
                    } else {
                        false
                    }
                }
                None => false,
            },
        }
    }

    fn seq(
        &self,
        v: &[Node],
        idx: usize,
        i: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(usize, &mut Caps) -> bool,
    ) -> bool {
        if idx == v.len() {
            return k(i, caps);
        }
        self.m(&v[idx], i, caps, &mut |j, caps: &mut Caps| {
            self.seq(v, idx + 1, j, caps, k)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn rep(
        &self,
        inner: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        count: usize,
        i: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(usize, &mut Caps) -> bool,
    ) -> bool {
        let can_more = max.is_none_or(|m| count < m);
        if !greedy && count >= min && k(i, caps) {
            return true;
        }
        if can_more {
            let more = self.m(inner, i, caps, &mut |j, caps: &mut Caps| {
                if j == i && count >= min {
                    return false;
                }
                self.rep(inner, min, max, greedy, count + 1, j, caps, k)
            });
            if more {
                return true;
            }
        }
        greedy && count >= min && k(i, caps)
    }
}

impl Regex {
    /// `re.compile(pattern)`; `verbose` is `re.VERBOSE`.
    pub(super) fn new(pattern: &str, verbose: bool) -> Regex {
        let p: Vec<char> = pattern.chars().collect();
        let mut parser = Parser {
            p: &p,
            i: 0,
            verbose,
            ngroups: 0,
            names: Vec::new(),
        };
        let root = parser.alt();
        assert!(
            parser.peek().is_none(),
            "trailing pattern text in {pattern:?}"
        );
        Regex {
            root,
            ngroups: parser.ngroups,
            names: parser.names,
        }
    }

    fn at(&self, t: &[char], start: usize) -> Option<Caps> {
        let ctx = Ctx { t };
        let mut caps: Caps = vec![None; self.ngroups + 1];
        let mut end = None;
        if ctx.m(&self.root, start, &mut caps, &mut |j, _| {
            end = Some(j);
            true
        }) {
            caps[0] = Some((start, end.unwrap()));
            Some(caps)
        } else {
            None
        }
    }

    fn wrap(&self, t: &[char], caps: Caps) -> Match {
        Match {
            caps,
            names: self.names.clone(),
            text: t.to_vec(),
        }
    }

    /// `pattern.match(text)`.
    pub(super) fn match_chars(&self, t: &[char]) -> Option<Match> {
        self.at(t, 0).map(|c| self.wrap(t, c))
    }

    /// `pattern.match(text)` on a `str`.
    pub(super) fn match_str(&self, s: &str) -> Option<Match> {
        let t: Vec<char> = s.chars().collect();
        self.match_chars(&t)
    }

    /// `pattern.match(text)` is not `None`.
    pub(super) fn is_match(&self, s: &str) -> bool {
        let t: Vec<char> = s.chars().collect();
        self.at(&t, 0).is_some()
    }

    /// `pattern.search(text)`.
    pub(super) fn search_chars(&self, t: &[char]) -> Option<Match> {
        for start in 0..=t.len() {
            if let Some(c) = self.at(t, start) {
                return Some(self.wrap(t, c));
            }
        }
        None
    }

    /// `pattern.search(text)` on a `str`.
    pub(super) fn search_str(&self, s: &str) -> Option<Match> {
        let t: Vec<char> = s.chars().collect();
        self.search_chars(&t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let r = Regex::new(r"(?P<a>\w+)_(?!x)", false);
        let m = r.search_str("  foo_bar").unwrap();
        assert_eq!(m.name("a").unwrap(), "foo");
        let r = Regex::new(r"([!-/:-@[-`{-~])\1* *$", false);
        assert!(r.is_match("=====  "));
        assert!(!r.is_match("==-"));
        let r = Regex::new(r"(?<!\\)(\\\\)*::$", false);
        assert!(r.search_str("text::").is_some());
        let r = Regex::new(r"(^|(?<=\s|[(]))x", false);
        assert_eq!(r.search_str("ax (x").unwrap().start(), 4);
    }
}
