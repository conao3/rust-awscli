//! `docutils/parsers/rst/states.py` `Inliner` (inline markup) and
//! `docutils/parsers/rst/roles.py` (interpreted text roles).

use super::nodes::{Id, Tag, Val, fully_normalize_name, unescape, whitespace_normalize_name};
use super::parser::{MsgArgs, P, R, SIMPLENAME, escape2null, split_escaped_whitespace};
use super::pyre::{Match, Regex};

/// `punctuation_chars.openers`.
pub(super) const OPENERS: &str = "\"'(<\\[{\u{0f3a}\u{0f3c}\u{169b}\u{2045}\u{207d}\u{208d}\u{2329}\u{2768}\u{276a}\u{276c}\u{276e}\u{2770}\u{2772}\u{2774}\u{27c5}\u{27e6}\u{27e8}\u{27ea}\u{27ec}\u{27ee}\u{2983}\u{2985}\u{2987}\u{2989}\u{298b}\u{298d}\u{298f}\u{2991}\u{2993}\u{2995}\u{2997}\u{29d8}\u{29da}\u{29fc}\u{2e22}\u{2e24}\u{2e26}\u{2e28}\u{3008}\u{300a}\u{300c}\u{300e}\u{3010}\u{3014}\u{3016}\u{3018}\u{301a}\u{301d}\u{301d}\u{fd3e}\u{fe17}\u{fe35}\u{fe37}\u{fe39}\u{fe3b}\u{fe3d}\u{fe3f}\u{fe41}\u{fe43}\u{fe47}\u{fe59}\u{fe5b}\u{fe5d}\u{ff08}\u{ff3b}\u{ff5b}\u{ff5f}\u{ff62}\u{ab}\u{2018}\u{201c}\u{2039}\u{2e02}\u{2e04}\u{2e09}\u{2e0c}\u{2e1c}\u{2e20}\u{201a}\u{201e}\u{bb}\u{2019}\u{201d}\u{203a}\u{2e03}\u{2e05}\u{2e0a}\u{2e0d}\u{2e1d}\u{2e21}\u{201b}\u{201f}";
/// `punctuation_chars.closers`.
pub(super) const CLOSERS: &str = "\"')>\\]}\u{0f3b}\u{0f3d}\u{169c}\u{2046}\u{207e}\u{208e}\u{232a}\u{2769}\u{276b}\u{276d}\u{276f}\u{2771}\u{2773}\u{2775}\u{27c6}\u{27e7}\u{27e9}\u{27eb}\u{27ed}\u{27ef}\u{2984}\u{2986}\u{2988}\u{298a}\u{298c}\u{298e}\u{2990}\u{2992}\u{2994}\u{2996}\u{2998}\u{29d9}\u{29db}\u{29fd}\u{2e23}\u{2e25}\u{2e27}\u{2e29}\u{3009}\u{300b}\u{300d}\u{300f}\u{3011}\u{3015}\u{3017}\u{3019}\u{301b}\u{301e}\u{301f}\u{fd3f}\u{fe18}\u{fe36}\u{fe38}\u{fe3a}\u{fe3c}\u{fe3e}\u{fe40}\u{fe42}\u{fe44}\u{fe48}\u{fe5a}\u{fe5c}\u{fe5e}\u{ff09}\u{ff3d}\u{ff5d}\u{ff60}\u{ff63}\u{bb}\u{2019}\u{201d}\u{203a}\u{2e03}\u{2e05}\u{2e0a}\u{2e0d}\u{2e1d}\u{2e21}\u{201b}\u{201f}\u{ab}\u{2018}\u{201c}\u{2039}\u{2e02}\u{2e04}\u{2e09}\u{2e0c}\u{2e1c}\u{2e20}\u{201a}\u{201e}";
/// `punctuation_chars.delimiters`.
const DELIMITERS: &str = "\\-/:\u{058a}\u{a1}\u{b7}\u{bf}\u{037e}\u{0387}\u{055a}-\u{055f}\u{0589}\u{05be}\u{05c0}\u{05c3}\u{05c6}\u{05f3}\u{05f4}\u{0609}\u{060a}\u{060c}\u{060d}\u{061b}\u{061e}\u{061f}\u{066a}-\u{066d}\u{06d4}\u{0700}-\u{070d}\u{07f7}-\u{07f9}\u{0830}-\u{083e}\u{0964}\u{0965}\u{0970}\u{0df4}\u{0e4f}\u{0e5a}\u{0e5b}\u{0f04}-\u{0f12}\u{0f85}\u{0fd0}-\u{0fd4}\u{104a}-\u{104f}\u{10fb}\u{1361}-\u{1368}\u{1400}\u{166d}\u{166e}\u{16eb}-\u{16ed}\u{1735}\u{1736}\u{17d4}-\u{17d6}\u{17d8}-\u{17da}\u{1800}-\u{180a}\u{1944}\u{1945}\u{19de}\u{19df}\u{1a1e}\u{1a1f}\u{1aa0}-\u{1aa6}\u{1aa8}-\u{1aad}\u{1b5a}-\u{1b60}\u{1c3b}-\u{1c3f}\u{1c7e}\u{1c7f}\u{1cd3}\u{2010}-\u{2017}\u{2020}-\u{2027}\u{2030}-\u{2038}\u{203b}-\u{203e}\u{2041}-\u{2043}\u{2047}-\u{2051}\u{2053}\u{2055}-\u{205e}\u{2cf9}-\u{2cfc}\u{2cfe}\u{2cff}\u{2e00}\u{2e01}\u{2e06}-\u{2e08}\u{2e0b}\u{2e0e}-\u{2e1b}\u{2e1e}\u{2e1f}\u{2e2a}-\u{2e2e}\u{2e30}\u{2e31}\u{3001}-\u{3003}\u{301c}\u{3030}\u{303d}\u{30a0}\u{30fb}\u{a4fe}\u{a4ff}\u{a60d}-\u{a60f}\u{a673}\u{a67e}\u{a6f2}-\u{a6f7}\u{a874}-\u{a877}\u{a8ce}\u{a8cf}\u{a8f8}-\u{a8fa}\u{a92e}\u{a92f}\u{a95f}\u{a9c1}-\u{a9cd}\u{a9de}\u{a9df}\u{aa5c}-\u{aa5f}\u{aade}\u{aadf}\u{abeb}\u{fe10}-\u{fe16}\u{fe19}\u{fe30}-\u{fe32}\u{fe45}\u{fe46}\u{fe49}-\u{fe4c}\u{fe50}-\u{fe52}\u{fe54}-\u{fe58}\u{fe5f}-\u{fe61}\u{fe63}\u{fe68}\u{fe6a}\u{fe6b}\u{ff01}-\u{ff03}\u{ff05}-\u{ff07}\u{ff0a}\u{ff0c}-\u{ff0f}\u{ff1a}\u{ff1b}\u{ff1f}\u{ff20}\u{ff3c}\u{ff61}\u{ff64}\u{ff65}\u{10100}\u{10101}\u{1039f}\u{103d0}\u{10857}\u{1091f}\u{1093f}\u{10a50}-\u{10a58}\u{10a7f}\u{10b39}-\u{10b3f}\u{110bb}\u{110bc}\u{110be}-\u{110c1}\u{12470}-\u{12473}";
/// `punctuation_chars.closing_delimiters`.
const CLOSING_DELIMITERS: &str = "\\\\.,;!?";

const URI_SCHEMES: &[&str] = &[
    "about",
    "acap",
    "addbook",
    "afp",
    "afs",
    "aim",
    "callto",
    "castanet",
    "chttp",
    "cid",
    "crid",
    "data",
    "dav",
    "dict",
    "dns",
    "eid",
    "fax",
    "feed",
    "file",
    "finger",
    "freenet",
    "ftp",
    "go",
    "gopher",
    "gsm-sms",
    "h323",
    "h324",
    "hdl",
    "hnews",
    "http",
    "https",
    "hydra",
    "iioploc",
    "ilu",
    "im",
    "imap",
    "info",
    "ior",
    "ipp",
    "irc",
    "iris.beep",
    "iseek",
    "jar",
    "javascript",
    "jdbc",
    "ldap",
    "lifn",
    "livescript",
    "lrq",
    "mailbox",
    "mailserver",
    "mailto",
    "md5",
    "mid",
    "mocha",
    "modem",
    "mtqp",
    "mupdate",
    "news",
    "nfs",
    "nntp",
    "opaquelocktoken",
    "phone",
    "pop",
    "pop3",
    "pres",
    "printer",
    "prospero",
    "rdar",
    "res",
    "rtsp",
    "rvp",
    "rwhois",
    "rx",
    "sdp",
    "service",
    "shttp",
    "sip",
    "sips",
    "smb",
    "snews",
    "snmp",
    "soap.beep",
    "soap.beeps",
    "ssh",
    "t120",
    "tag",
    "tcp",
    "tel",
    "telephone",
    "telnet",
    "tftp",
    "tip",
    "tn3270",
    "tv",
    "urn",
    "uuid",
    "vemmi",
    "videotex",
    "view-source",
    "wais",
    "whodp",
    "whois++",
    "x-man-page",
    "xmlrpc.beep",
    "xmlrpc.beeps",
    "z39.50r",
    "z39.50s",
];

/// `punctuation_chars.match_chars`.
fn match_chars(c1: char, c2: char) -> bool {
    let Some(i) = OPENERS.chars().position(|c| c == c1) else {
        return false;
    };
    if CLOSERS.chars().nth(i) == Some(c2) {
        return true;
    }
    let pairs: &str = match c1 {
        '\u{bb}' => "\u{bb}",
        '\u{2018}' => "\u{201a}",
        '\u{2019}' => "\u{2019}",
        '\u{201a}' => "\u{2018}\u{2019}",
        '\u{201c}' => "\u{201e}",
        '\u{201e}' => "\u{201c}\u{201d}",
        '\u{201d}' => "\u{201d}",
        '\u{203a}' => "\u{203a}",
        _ => "",
    };
    pairs.contains(c2)
}

/// `Inliner.patterns`.
pub(super) struct InlinePatterns {
    pub initial: Regex,
    pub emphasis: Regex,
    pub strong: Regex,
    pub interpreted_or_phrase_ref: Regex,
    pub embedded_link: Regex,
    pub literal: Regex,
    pub target: Regex,
    pub substitution_ref: Regex,
    pub email: Regex,
    pub uri: Regex,
}

impl InlinePatterns {
    pub(super) fn new() -> InlinePatterns {
        let start_string_prefix = format!(r"(^|(?<=\s|[{OPENERS}{DELIMITERS}]))");
        let end_string_suffix =
            format!("($|(?=\\s|[\x00{CLOSING_DELIMITERS}{DELIMITERS}{CLOSERS}]))");
        let non_whitespace_before = r"(?<!\s)";
        let non_whitespace_escape_before = r"(?<![\s\x00])";
        let non_unescaped_whitespace_escape_before = r"(?<!(?<!\x00)[\s\x00])";
        let non_whitespace_after = r"(?!\s)";
        let uric = r"[-_.!~*'()[\];/:@&=+$,%a-zA-Z0-9\x00]";
        let urilast = r"[_~*/=+a-zA-Z0-9]";
        let uri_end = format!(r"(?:{urilast}|{uric}(?=[>]))");
        let emailc = r"[-_!~*'{|}/#?^`&=+$%a-zA-Z0-9\x00]";
        let email_pattern = format!(
            r"
          {emailc}+(?:\.{emailc}+)*   # name
          (?<!\x00)@                      # at
          {emailc}+(?:\.{emailc}*)*   # host
          {uri_end}                     # final URI char
          "
        );
        let initial = format!(
            r"{start_string_prefix}(?P<initial_inline>(?P<start>\*\*|\*(?!\*)|``|_`|\|(?!\|)){non_whitespace_after}|(?P<whole>(?P<refname>{SIMPLENAME})(?P<refend>__?)|\[(?P<footnotelabel>[0-9]+|\#({SIMPLENAME})?|\*|(?P<citationlabel>{SIMPLENAME}))(?P<fnend>\]_)){end_string_suffix}|(?P<role>(:{SIMPLENAME}:)?)(?P<backquote>`(?!`)){non_whitespace_after})"
        );
        InlinePatterns {
            initial: Regex::new(&initial, false),
            emphasis: Regex::new(
                &format!(r"{non_whitespace_escape_before}(\*){end_string_suffix}"),
                false,
            ),
            strong: Regex::new(
                &format!(r"{non_whitespace_escape_before}(\*\*){end_string_suffix}"),
                false,
            ),
            interpreted_or_phrase_ref: Regex::new(
                &format!(
                    r"
              {non_unescaped_whitespace_escape_before}
              (
                `
                (?P<suffix>
                  (?P<role>:{SIMPLENAME}:)?
                  (?P<refend>__?)?
                )
              )
              {end_string_suffix}
              "
                ),
                true,
            ),
            embedded_link: Regex::new(
                &format!(
                    r"
              (
                (?:[ \n]+|^)            # spaces or beginning of line/string
                <                       # open bracket
                {non_whitespace_after}
                (([^<>]|\x00[<>])+)     # anything but unescaped angle brackets
                {non_whitespace_escape_before}
                >                       # close bracket
              )
              $                         # end of string
              "
                ),
                true,
            ),
            literal: Regex::new(
                &format!(r"{non_whitespace_before}(``){end_string_suffix}"),
                false,
            ),
            target: Regex::new(
                &format!(r"{non_whitespace_escape_before}(`){end_string_suffix}"),
                false,
            ),
            substitution_ref: Regex::new(
                &format!(r"{non_whitespace_escape_before}(\|_{{0,2}}){end_string_suffix}"),
                false,
            ),
            email: Regex::new(&format!("{email_pattern}$"), true),
            uri: Regex::new(
                &format!(
                    r"
                {start_string_prefix}
                (?P<whole>
                  (?P<absolute>           # absolute URI
                    (?P<scheme>             # scheme (http, ftp, mailto)
                      [a-zA-Z][a-zA-Z0-9.+-]*
                    )
                    :
                    (
                      (                       # either:
                        (//?)?                  # hierarchical URI
                        {uric}*               # URI characters
                        {uri_end}             # final URI char
                      )
                      (                       # optional query
                        \?{uric}*
                        {uri_end}
                      )?
                      (                       # optional fragment
                        \#{uric}*
                        {uri_end}
                      )?
                    )
                  )
                |                       # *OR*
                  (?P<email>              # email address
                    {email_pattern}
                  )
                )
                {end_string_suffix}
                "
                ),
                true,
            ),
        }
    }
}

type Parsed = (Vec<char>, Vec<Id>, Vec<char>, Vec<Id>);

fn s(v: &[char]) -> String {
    v.iter().collect()
}

impl P {
    /// `Inliner.parse`.
    pub(super) fn inline_parse(
        &mut self,
        text: &str,
        lineno: isize,
        parent: Id,
    ) -> R<(Vec<Id>, Vec<Id>)> {
        let pats = &self.pats.inline;
        let mut remaining: Vec<char> = escape2null(text).chars().collect();
        let mut processed = Vec::new();
        let mut unprocessed: Vec<char> = Vec::new();
        let mut messages = Vec::new();
        while !remaining.is_empty() {
            let Some(m) = pats.initial.search_chars(&remaining) else {
                break;
            };
            let key = ["start", "backquote", "refend", "fnend"]
                .iter()
                .find_map(|k| m.name(k).filter(|v| !v.is_empty()))
                .unwrap_or_default();
            let (before, inlines, rem, sysmessages) = match key.as_str() {
                "*" => self.inline_obj(&m, lineno, Kind::Emphasis, parent)?.0,
                "**" => self.inline_obj(&m, lineno, Kind::Strong, parent)?.0,
                "``" => self.inline_obj(&m, lineno, Kind::Literal, parent)?.0,
                "_`" => self.inline_internal_target(&m, lineno, parent)?,
                "|" => self.substitution_reference(&m, lineno, parent)?,
                "`" => self.interpreted_or_phrase_ref(&m, lineno, parent)?,
                "]_" => self.footnote_reference(&m),
                "_" => self.reference(&m, false),
                "__" => self.reference(&m, true),
                _ => unreachable!("inline key {key:?}"),
            };
            unprocessed.extend(before);
            messages.extend(sysmessages);
            if !inlines.is_empty() {
                let pending = std::mem::take(&mut unprocessed);
                processed.extend(self.implicit_inline(&pending));
                processed.extend(inlines);
            }
            remaining = rem;
        }
        let mut rest = unprocessed;
        rest.extend(remaining);
        if !rest.is_empty() {
            processed.extend(self.implicit_inline(&rest));
        }
        Ok((processed, messages))
    }

    /// `Inliner.quoted_start`.
    fn quoted_start(m: &Match) -> bool {
        let string = m.string();
        let start = m.start();
        if start == 0 {
            return false;
        }
        let prestart = string[start - 1];
        match string.get(m.end()) {
            None => true,
            Some(&poststart) => match_chars(prestart, poststart),
        }
    }

    /// `Inliner.problematic`.
    pub(super) fn problematic(&mut self, text: &str, rawsource: &str, message: Id) -> Id {
        let msgid = self.doc.set_id(message);
        let prb = self.doc.text_elem(Tag::Problematic, rawsource, text);
        self.doc.set_str(prb, "refid", &msgid);
        let prbid = self.doc.set_id(prb);
        self.doc.add_backref(message, &prbid);
        prb
    }

    /// `Inliner.inline_obj`; the second value is the end string.
    fn inline_obj(
        &mut self,
        m: &Match,
        lineno: isize,
        kind: Kind,
        _parent: Id,
    ) -> R<(Parsed, String)> {
        let string = m.string().to_vec();
        let (matchstart, matchend) = m.span_n("start").unwrap();
        if Self::quoted_start(m) {
            return Ok((
                (
                    string[..matchend].to_vec(),
                    vec![],
                    string[matchend..].to_vec(),
                    vec![],
                ),
                String::new(),
            ));
        }
        let pats = &self.pats.inline;
        let end_pattern = match kind {
            Kind::Emphasis => &pats.emphasis,
            Kind::Strong => &pats.strong,
            Kind::Literal => &pats.literal,
            Kind::Target => &pats.target,
            Kind::SubstitutionReference => &pats.substitution_ref,
        };
        let tail = &string[matchend..];
        if let Some(em) = end_pattern.search_chars(tail)
            && em.span(1).unwrap().0 > 0
        {
            let (s1, e1) = em.span(1).unwrap();
            let mut text = s(&tail[..s1]);
            if kind == Kind::Literal {
                text = unescape(&text, true);
            }
            let textend = matchend + e1;
            let rawsource = unescape(&s(&string[matchstart..textend]), true);
            let node = self.doc.text_elem(kind.tag(), &rawsource, &text);
            return Ok((
                (
                    string[..matchstart].to_vec(),
                    vec![node],
                    string[textend..].to_vec(),
                    vec![],
                ),
                em.group(1).unwrap(),
            ));
        }
        let msg = self.warning(
            &format!(
                "Inline {} start-string without end-string.",
                kind.tag().name()
            ),
            MsgArgs::line(lineno),
        )?;
        let text = unescape(&s(&string[matchstart..matchend]), true);
        let prb = self.problematic(&text, &text, msg);
        Ok((
            (
                string[..matchstart].to_vec(),
                vec![prb],
                string[matchend..].to_vec(),
                vec![msg],
            ),
            String::new(),
        ))
    }

    /// `Inliner.inline_internal_target`.
    fn inline_internal_target(&mut self, m: &Match, lineno: isize, parent: Id) -> R<Parsed> {
        let (r, _) = self.inline_obj(m, lineno, Kind::Target, parent)?;
        if let Some(&t) = r.1.first()
            && self.doc.tag(t) == Tag::Target
        {
            let name = fully_normalize_name(&self.doc.astext(t));
            self.doc.push_list(t, "names", &name);
            self.note_explicit_target(t, Some(parent))?;
        }
        Ok(r)
    }

    /// `Inliner.substitution_reference`.
    fn substitution_reference(&mut self, m: &Match, lineno: isize, parent: Id) -> R<Parsed> {
        let (mut r, endstring) = self.inline_obj(m, lineno, Kind::SubstitutionReference, parent)?;
        if r.1.len() == 1 {
            let subref = r.1[0];
            if self.doc.tag(subref) == Tag::SubstitutionReference {
                let subref_text = self.doc.astext(subref);
                self.doc.note_substitution_ref(subref, &subref_text);
                if endstring.ends_with('_') {
                    let rn = self
                        .doc
                        .elem(Tag::Reference, &format!("|{subref_text}{endstring}"));
                    if endstring.ends_with("__") {
                        self.doc.set(rn, "anonymous", Val::Int(1));
                    } else {
                        self.doc
                            .set_str(rn, "refname", &fully_normalize_name(&subref_text));
                        self.doc.note_refname(rn);
                    }
                    self.doc.append(rn, subref);
                    r.1 = vec![rn];
                }
            }
        }
        Ok(r)
    }

    /// `Inliner.interpreted_or_phrase_ref`.
    fn interpreted_or_phrase_ref(&mut self, m: &Match, lineno: isize, parent: Id) -> R<Parsed> {
        let string = m.string().to_vec();
        let (matchstart, matchend) = m.span_n("backquote").unwrap();
        let rolestart = m.span_n("role").unwrap().0;
        let mut role = m.name("role").unwrap_or_default();
        let mut position = "";
        if !role.is_empty() {
            let rc: Vec<char> = role.chars().collect();
            role = s(&rc[1..rc.len() - 1]);
            position = "prefix";
        } else if Self::quoted_start(m) {
            return Ok((
                string[..matchend].to_vec(),
                vec![],
                string[matchend..].to_vec(),
                vec![],
            ));
        }
        let tail = &string[matchend..];
        if let Some(em) = self
            .pats
            .inline
            .interpreted_or_phrase_ref
            .search_chars(tail)
            && em.span(1).unwrap().0 > 0
        {
            let textend = matchend + em.end();
            if em.has("role") {
                if !role.is_empty() {
                    let msg = self.warning(
                        "Multiple roles in interpreted text (both prefix and suffix present; only one allowed).",
                        MsgArgs::line(lineno),
                    )?;
                    let text = unescape(&s(&string[rolestart..textend]), true);
                    let prb = self.problematic(&text, &text, msg);
                    return Ok((
                        string[..rolestart].to_vec(),
                        vec![prb],
                        string[textend..].to_vec(),
                        vec![msg],
                    ));
                }
                let suffix: Vec<char> = em.name("suffix").unwrap().chars().collect();
                role = s(&suffix[1..suffix.len() - 1]);
                position = "suffix";
            }
            let escaped = s(&tail[..em.span(1).unwrap().0]);
            let rawsource = unescape(&s(&string[matchstart..textend]), true);
            if rawsource.ends_with('_') {
                if !role.is_empty() {
                    let msg = self.warning(
                        &format!(
                            "Mismatch: both interpreted text role {position} and reference suffix."
                        ),
                        MsgArgs::line(lineno),
                    )?;
                    let text = unescape(&s(&string[rolestart..textend]), true);
                    let prb = self.problematic(&text, &text, msg);
                    return Ok((
                        string[..rolestart].to_vec(),
                        vec![prb],
                        string[textend..].to_vec(),
                        vec![msg],
                    ));
                }
                return self.phrase_ref(
                    string[..matchstart].to_vec(),
                    string[textend..].to_vec(),
                    &rawsource,
                    &escaped,
                    parent,
                );
            }
            let rawsource = unescape(&s(&string[rolestart..textend]), true);
            let (nodelist, messages) = self.interpreted(&rawsource, &escaped, &role, lineno)?;
            return Ok((
                string[..rolestart].to_vec(),
                nodelist,
                string[textend..].to_vec(),
                messages,
            ));
        }
        let msg = self.warning(
            "Inline interpreted text or phrase reference start-string without end-string.",
            MsgArgs::line(lineno),
        )?;
        let text = unescape(&s(&string[matchstart..matchend]), true);
        let prb = self.problematic(&text, &text, msg);
        Ok((
            string[..matchstart].to_vec(),
            vec![prb],
            string[matchend..].to_vec(),
            vec![msg],
        ))
    }

    /// `Inliner.phrase_ref`.
    fn phrase_ref(
        &mut self,
        before: Vec<char>,
        after: Vec<char>,
        rawsource: &str,
        escaped: &str,
        parent: Id,
    ) -> R<Parsed> {
        let ec: Vec<char> = escaped.chars().collect();
        let mut target = None;
        let mut aliastype = "";
        let mut alias = String::new();
        let (text, unescaped, rawtext) =
            if let Some(m) = self.pats.inline.embedded_link.search_chars(&ec) {
                let mut text = s(&ec[..m.start()]);
                let mut unescaped = unescape(&text, false);
                let mut rawtext = unescape(&text, true);
                let aliastext = m.group(2).unwrap();
                let rawaliastext = unescape(&aliastext, true);
                let underscore_escaped = rawaliastext.ends_with("\\_");
                let g1 = m.group(1).unwrap();
                if aliastext.ends_with('_')
                    && !(underscore_escaped || self.pats.inline.uri.match_str(&aliastext).is_some())
                {
                    aliastype = "name";
                    let a: Vec<char> = aliastext.chars().collect();
                    let inner = s(&a[..a.len() - 1]);
                    alias = fully_normalize_name(&unescape(&inner, false));
                    let t = self.doc.elem(Tag::Target, &g1);
                    self.doc.set_str(t, "refname", &alias);
                    target = Some(t);
                } else {
                    aliastype = "uri";
                    let parts = split_escaped_whitespace(&aliastext);
                    let joined = parts
                        .iter()
                        .map(|p| super::chars::split_ws(p).join(""))
                        .collect::<Vec<_>>()
                        .join(" ");
                    alias = self.adjust_uri(&unescape(&joined, false));
                    if alias.ends_with("\\_") {
                        alias.truncate(alias.len() - 2);
                        alias.push('_');
                    }
                    let t = self.doc.elem(Tag::Target, &g1);
                    self.doc.set_str(t, "refuri", &alias);
                    target = Some(t);
                }
                if text.is_empty() {
                    text = alias.clone();
                    unescaped = unescape(&text, false);
                    rawtext = rawaliastext;
                }
                (text, unescaped, rawtext)
            } else {
                (
                    escaped.to_string(),
                    unescape(escaped, false),
                    unescape(escaped, true),
                )
            };
        let _ = rawtext;
        let refname = fully_normalize_name(&unescaped);
        let reference = self.doc.text_elem(Tag::Reference, rawsource, &text);
        self.doc
            .set_str(reference, "name", &whitespace_normalize_name(&unescaped));
        let mut node_list = vec![reference];
        if rawsource.ends_with("__") {
            if target.is_some() && aliastype == "name" {
                self.doc.set_str(reference, "refname", &alias);
                self.doc.note_refname(reference);
            } else if target.is_some() && aliastype == "uri" {
                self.doc.set_str(reference, "refuri", &alias);
            } else {
                self.doc.set(reference, "anonymous", Val::Int(1));
            }
        } else if let Some(t) = target {
            self.doc.push_list(t, "names", &refname);
            if aliastype == "name" {
                self.doc.set_str(reference, "refname", &alias);
                self.doc.note_indirect_target(t);
                self.doc.note_refname(reference);
            } else {
                self.doc.set_str(reference, "refuri", &alias);
                self.note_explicit_target(t, Some(parent))?;
            }
            node_list.push(t);
        } else {
            self.doc.set_str(reference, "refname", &refname);
            self.doc.note_refname(reference);
        }
        Ok((before, node_list, after, vec![]))
    }

    /// `Inliner.adjust_uri`.
    pub(super) fn adjust_uri(&self, uri: &str) -> String {
        if self.pats.inline.email.match_str(uri).is_some() {
            format!("mailto:{uri}")
        } else {
            uri.to_string()
        }
    }

    /// `Inliner.interpreted` with `roles.role`.
    fn interpreted(
        &mut self,
        rawsource: &str,
        text: &str,
        role: &str,
        lineno: isize,
    ) -> R<(Vec<Id>, Vec<Id>)> {
        let normname = role.to_lowercase();
        let canonical: Option<&str> = if role.is_empty() {
            Some("title-reference")
        } else {
            match normname.as_str() {
                "abbreviation" | "ab" => Some("abbreviation"),
                "acronym" | "ac" => Some("acronym"),
                "code" => Some("code"),
                "index" | "i" => Some("index"),
                "subscript" | "sub" => Some("subscript"),
                "superscript" | "sup" => Some("superscript"),
                "title-reference" | "title" | "t" => Some("title-reference"),
                "pep-reference" | "pep" => Some("pep-reference"),
                "rfc-reference" | "rfc" => Some("rfc-reference"),
                "emphasis" => Some("emphasis"),
                "strong" => Some("strong"),
                "literal" => Some("literal"),
                "math" => Some("math"),
                "named-reference" => Some("named-reference"),
                "anonymous-reference" => Some("anonymous-reference"),
                "footnote-reference" => Some("footnote-reference"),
                "citation-reference" => Some("citation-reference"),
                "substitution-reference" => Some("substitution-reference"),
                "target" => Some("target"),
                "uri-reference" | "uri" | "url" => Some("uri-reference"),
                "raw" => Some("raw"),
                _ => None,
            }
        };
        let mut messages = Vec::new();
        let canonical = match canonical {
            Some(c) => c.to_string(),
            None => {
                let msg = self.info(
                    &format!(
                        "No role entry for \"{role}\" in module \"docutils.parsers.rst.languages.en\".\nTrying \"{role}\" as canonical role name."
                    ),
                    MsgArgs::line(lineno),
                )?;
                messages.push(msg);
                normname.clone()
            }
        };
        let generic = |tag: Tag| Some(tag);
        let tag = match canonical.as_str() {
            "abbreviation" => generic(Tag::Abbreviation),
            "acronym" => generic(Tag::Acronym),
            "emphasis" => generic(Tag::Emphasis),
            "literal" => generic(Tag::Literal),
            "strong" => generic(Tag::Strong),
            "subscript" => generic(Tag::Subscript),
            "superscript" => generic(Tag::Superscript),
            "title-reference" => generic(Tag::TitleReference),
            _ => None,
        };
        if let Some(tag) = tag {
            let n = self.doc.text_elem(tag, rawsource, text);
            return Ok((vec![n], messages));
        }
        match canonical.as_str() {
            "code" => {
                let n = self.doc.elem(Tag::Literal, rawsource);
                self.doc.set(n, "classes", Val::List(vec!["code".into()]));
                let value = unescape(text, true);
                if !value.is_empty() {
                    let t = self.doc.text(&value);
                    self.doc.append(n, t);
                }
                Ok((vec![n], messages))
            }
            "math" => {
                let n = self
                    .doc
                    .text_elem(Tag::Math, rawsource, &unescape(text, true));
                Ok((vec![n], messages))
            }
            "pep-reference" => {
                let num = unescape(text, false)
                    .parse::<i64>()
                    .ok()
                    .filter(|n| (0..=9999).contains(n));
                match num {
                    Some(n) => {
                        let r =
                            self.doc
                                .text_elem(Tag::Reference, rawsource, &format!("PEP {text}"));
                        self.doc.set_str(
                            r,
                            "refuri",
                            &format!("https://peps.python.org/pep-{n:04}"),
                        );
                        Ok((vec![r], messages))
                    }
                    None => {
                        let msg = self.error(
                            &format!(
                                "PEP number must be a number from 0 to 9999; \"{text}\" is invalid."
                            ),
                            MsgArgs::line(lineno),
                        )?;
                        let prb = self.problematic(rawsource, rawsource, msg);
                        messages.push(msg);
                        Ok((vec![prb], messages))
                    }
                }
            }
            "rfc-reference" => {
                let un = unescape(text, false);
                let (num, section) = match un.split_once('#') {
                    Some((a, b)) => (a.to_string(), Some(b.to_string())),
                    None => (un.clone(), None),
                };
                match num.parse::<i64>().ok().filter(|n| *n >= 1) {
                    Some(n) => {
                        let mut uri = format!("https://tools.ietf.org/html/rfc{n}.html");
                        if let Some(sct) = section {
                            uri.push('#');
                            uri.push_str(&sct);
                        }
                        let r = self
                            .doc
                            .text_elem(Tag::Reference, rawsource, &format!("RFC {n}"));
                        self.doc.set_str(r, "refuri", &uri);
                        Ok((vec![r], messages))
                    }
                    None => {
                        let msg = self.error(
                            &format!(
                                "RFC number must be a number greater than or equal to 1; \"{text}\" is invalid."
                            ),
                            MsgArgs::line(lineno),
                        )?;
                        let prb = self.problematic(rawsource, rawsource, msg);
                        messages.push(msg);
                        Ok((vec![prb], messages))
                    }
                }
            }
            "raw" => {
                let msg = self.error(
                    &format!(
                        "No format (Writer name) is associated with this role: \"{role}\".\nThe \"raw\" role cannot be used directly.\nInstead, use the \"role\" directive to create a new role with an associated format."
                    ),
                    MsgArgs::line(lineno),
                )?;
                let prb = self.problematic(rawsource, rawsource, msg);
                messages.push(msg);
                Ok((vec![prb], messages))
            }
            "index"
            | "named-reference"
            | "anonymous-reference"
            | "uri-reference"
            | "footnote-reference"
            | "citation-reference"
            | "substitution-reference"
            | "target" => {
                let msg = self.error(
                    &format!("Interpreted text role \"{role}\" not implemented."),
                    MsgArgs::line(lineno),
                )?;
                let prb = self.problematic(rawsource, rawsource, msg);
                messages.push(msg);
                Ok((vec![prb], messages))
            }
            _ => {
                let msg = self.error(
                    &format!("Unknown interpreted text role \"{role}\"."),
                    MsgArgs::line(lineno),
                )?;
                let prb = self.problematic(rawsource, rawsource, msg);
                messages.push(msg);
                Ok((vec![prb], messages))
            }
        }
    }

    /// `Inliner.footnote_reference`.
    fn footnote_reference(&mut self, m: &Match) -> Parsed {
        let label = m.name("footnotelabel").unwrap();
        let mut refname = fully_normalize_name(&label);
        let string = m.string();
        let (ws, we) = m.span_n("whole").unwrap();
        let before = string[..ws].to_vec();
        let remaining = string[we..].to_vec();
        let refnode;
        if m.has("citationlabel") {
            refnode = self
                .doc
                .elem(Tag::CitationReference, &format!("[{label}]_"));
            self.doc.set_str(refnode, "refname", &refname);
            let t = self.doc.text(&label);
            self.doc.append(refnode, t);
            self.doc.set_id(refnode);
            self.doc
                .citation_refs
                .entry(refname.clone())
                .or_default()
                .push(refnode);
            self.doc.note_refname(refnode);
        } else {
            refnode = self
                .doc
                .elem(Tag::FootnoteReference, &format!("[{label}]_"));
            if let Some(r) = refname.strip_prefix('#') {
                refname = r.to_string();
                self.doc.set(refnode, "auto", Val::Int(1));
                self.doc.set_id(refnode);
                self.doc.autofootnote_refs.push(refnode);
            } else if refname == "*" {
                refname = String::new();
                self.doc.set_str(refnode, "auto", "*");
                self.doc.set_id(refnode);
                self.doc.symbol_footnote_refs.push(refnode);
            } else {
                let t = self.doc.text(&label);
                self.doc.append(refnode, t);
            }
            if !refname.is_empty() {
                self.doc.set_str(refnode, "refname", &refname);
                self.doc.set_id(refnode);
                self.doc
                    .footnote_refs
                    .entry(refname.clone())
                    .or_default()
                    .push(refnode);
                self.doc.note_refname(refnode);
            }
        }
        (before, vec![refnode], remaining, vec![])
    }

    /// `Inliner.reference` / `anonymous_reference`.
    fn reference(&mut self, m: &Match, anonymous: bool) -> Parsed {
        let referencename = m.name("refname").unwrap();
        let refname = fully_normalize_name(&referencename);
        let refend = m.name("refend").unwrap();
        let node = self.doc.text_elem(
            Tag::Reference,
            &format!("{referencename}{refend}"),
            &referencename,
        );
        self.doc
            .set_str(node, "name", &whitespace_normalize_name(&referencename));
        if anonymous {
            self.doc.set(node, "anonymous", Val::Int(1));
        } else {
            self.doc.set_str(node, "refname", &refname);
            self.doc.note_refname(node);
        }
        let string = m.string();
        let (ws, we) = m.span_n("whole").unwrap();
        (
            string[..ws].to_vec(),
            vec![node],
            string[we..].to_vec(),
            vec![],
        )
    }

    /// `Inliner.implicit_inline` (standalone URIs).
    fn implicit_inline(&mut self, text: &[char]) -> Vec<Id> {
        if text.is_empty() {
            return Vec::new();
        }
        if let Some(m) = self.pats.inline.uri.search_chars(text) {
            let scheme_ok = match m.name("scheme") {
                None => true,
                Some(sch) => URI_SCHEMES.contains(&sch.to_lowercase().as_str()),
            };
            if scheme_ok {
                let mut out = self.implicit_inline(&text[..m.start()]);
                let whole = m.name("whole").unwrap();
                let addscheme = if m.span_n("email").is_some() {
                    "mailto:"
                } else {
                    ""
                };
                let refuri = format!("{addscheme}{}", unescape(&whole, false));
                let r = self
                    .doc
                    .text_elem(Tag::Reference, &unescape(&whole, true), &whole);
                self.doc.set_str(r, "refuri", &refuri);
                out.push(r);
                out.extend(self.implicit_inline(&text[m.end()..]));
                return out;
            }
        }
        vec![self.doc.text(&s(text))]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Emphasis,
    Strong,
    Literal,
    Target,
    SubstitutionReference,
}

impl Kind {
    fn tag(self) -> Tag {
        match self {
            Kind::Emphasis => Tag::Emphasis,
            Kind::Strong => Tag::Strong,
            Kind::Literal => Tag::Literal,
            Kind::Target => Tag::Target,
            Kind::SubstitutionReference => Tag::SubstitutionReference,
        }
    }
}
