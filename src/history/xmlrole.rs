//! expat's tokenizer of the prolog (`xmltok_impl.c` `prologTok` with the
//! scanners it calls, in the namespace-processing encoding) and its role
//! state machine (`xmlrole.c`), over the characters of the document.

use super::minidom::{NAME_CHAR, NAME_START, in_ranges};

/// `BYTE_TYPE` of a character: the ASCII classes of `asciitab.h` (with
/// `:` as `BT_COLON`), `NonXml` for the characters that are not XML
/// characters, and `NonAscii` for the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bt {
    NonXml,
    Lt,
    Amp,
    Rsqb,
    Cr,
    Lf,
    Gt,
    Quot,
    Apos,
    Equals,
    Quest,
    Excl,
    Sol,
    Semi,
    Num,
    Lsqb,
    S,
    Nmstrt,
    Colon,
    Hex,
    Digit,
    Name,
    Minus,
    Other,
    NonAscii,
    Percnt,
    Lpar,
    Rpar,
    Ast,
    Plus,
    Comma,
    Verbar,
}

pub(crate) fn bt(c: char) -> Bt {
    match c {
        '\t' | ' ' => Bt::S,
        '\n' => Bt::Lf,
        '\r' => Bt::Cr,
        '\0'..='\u{1f}' => Bt::NonXml,
        '!' => Bt::Excl,
        '"' => Bt::Quot,
        '#' => Bt::Num,
        '%' => Bt::Percnt,
        '&' => Bt::Amp,
        '\'' => Bt::Apos,
        '(' => Bt::Lpar,
        ')' => Bt::Rpar,
        '*' => Bt::Ast,
        '+' => Bt::Plus,
        ',' => Bt::Comma,
        '-' => Bt::Minus,
        '.' => Bt::Name,
        '/' => Bt::Sol,
        '0'..='9' => Bt::Digit,
        ':' => Bt::Colon,
        ';' => Bt::Semi,
        '<' => Bt::Lt,
        '=' => Bt::Equals,
        '>' => Bt::Gt,
        '?' => Bt::Quest,
        'a'..='f' | 'A'..='F' => Bt::Hex,
        'g'..='z' | 'G'..='Z' | '_' => Bt::Nmstrt,
        '[' => Bt::Lsqb,
        ']' => Bt::Rsqb,
        '|' => Bt::Verbar,
        '$' | '@' | '\\' | '^' | '`' | '{' | '}' | '~' | '\u{7f}' => Bt::Other,
        '\u{fffe}' | '\u{ffff}' => Bt::NonXml,
        _ => Bt::NonAscii,
    }
}

/// `IS_NMSTRT_CHAR` (with the ASCII classes that start a name).
pub(crate) fn is_nmstrt(c: char) -> bool {
    match bt(c) {
        Bt::Nmstrt | Bt::Hex => true,
        Bt::NonAscii => in_ranges(NAME_START, c),
        _ => false,
    }
}

/// `CHECK_NAME_CASES`: a character that continues a name.
pub(crate) fn is_namechar(c: char) -> bool {
    match bt(c) {
        Bt::Nmstrt | Bt::Hex | Bt::Digit | Bt::Name | Bt::Minus => true,
        Bt::NonAscii => in_ranges(NAME_CHAR, c),
        _ => false,
    }
}

/// `INVALID_CASES`: a character that is not an XML character.
pub(crate) fn is_invalid(c: char) -> bool {
    bt(c) == Bt::NonXml
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tok {
    /// `XML_TOK_INVALID`, at the position the scan stopped.
    Invalid,
    /// `XML_TOK_PARTIAL`: an unclosed token.
    Partial,
    None,
    PrologS,
    XmlDecl,
    Pi,
    Comment,
    DeclOpen,
    CondSectOpen,
    CondSectClose,
    InstanceStart,
    Literal,
    Name,
    PrefixedName,
    Nmtoken,
    NameQuestion,
    NameAsterisk,
    NamePlus,
    PoundName,
    Percent,
    ParamEntityRef,
    OpenBracket,
    CloseBracket,
    OpenParen,
    CloseParen,
    CloseParenQuestion,
    CloseParenAsterisk,
    CloseParenPlus,
    Or,
    Comma,
    DeclClose,
}

/// `scanComment` from after `<!-`.
pub(crate) fn scan_comment(s: &[char], mut p: usize) -> (Tok, usize) {
    if p < s.len() {
        if s[p] != '-' {
            return (Tok::Invalid, p);
        }
        p += 1;
        while p < s.len() {
            match s[p] {
                c if is_invalid(c) => return (Tok::Invalid, p),
                '-' => {
                    p += 1;
                    if p >= s.len() {
                        return (Tok::Partial, p);
                    }
                    if s[p] == '-' {
                        p += 1;
                        if p >= s.len() {
                            return (Tok::Partial, p);
                        }
                        if s[p] != '>' {
                            return (Tok::Invalid, p);
                        }
                        return (Tok::Comment, p + 1);
                    }
                }
                _ => p += 1,
            }
        }
    }
    (Tok::Partial, p)
}

/// `scanDecl` from after `<!`.
fn scan_decl(s: &[char], mut p: usize) -> (Tok, usize) {
    let Some(&c) = s.get(p) else {
        return (Tok::Partial, p);
    };
    match bt(c) {
        Bt::Minus => return scan_comment(s, p + 1),
        Bt::Lsqb => return (Tok::CondSectOpen, p + 1),
        Bt::Nmstrt | Bt::Hex => p += 1,
        _ => return (Tok::Invalid, p),
    }
    while let Some(&c) = s.get(p) {
        match bt(c) {
            Bt::Percnt => {
                let Some(&n) = s.get(p + 1) else {
                    return (Tok::Partial, p);
                };
                if matches!(bt(n), Bt::S | Bt::Cr | Bt::Lf | Bt::Percnt) {
                    return (Tok::Invalid, p);
                }
                return (Tok::DeclOpen, p);
            }
            Bt::S | Bt::Cr | Bt::Lf => return (Tok::DeclOpen, p),
            Bt::Nmstrt | Bt::Hex => p += 1,
            _ => return (Tok::Invalid, p),
        }
    }
    (Tok::Partial, p)
}

/// `checkPiTarget`: `Some(XmlDecl)` for `xml`, `None` for another case
/// of it, else `Some(Pi)`.
fn check_pi_target(target: &[char]) -> Option<Tok> {
    if target.len() != 3 {
        return Some(Tok::Pi);
    }
    let lower: String = target.iter().collect::<String>().to_ascii_lowercase();
    if lower != "xml" {
        return Some(Tok::Pi);
    }
    if target.iter().any(|c| c.is_ascii_uppercase()) {
        None
    } else {
        Some(Tok::XmlDecl)
    }
}

/// `scanPi` from after `<?`.
pub(crate) fn scan_pi(s: &[char], mut p: usize) -> (Tok, usize) {
    let target = p;
    let Some(&c) = s.get(p) else {
        return (Tok::Partial, p);
    };
    if !is_nmstrt(c) {
        return (Tok::Invalid, p);
    }
    p += 1;
    while let Some(&c) = s.get(p) {
        match bt(c) {
            Bt::S | Bt::Cr | Bt::Lf => {
                let Some(tok) = check_pi_target(&s[target..p]) else {
                    return (Tok::Invalid, p);
                };
                p += 1;
                while let Some(&c) = s.get(p) {
                    if is_invalid(c) {
                        return (Tok::Invalid, p);
                    }
                    p += 1;
                    if c == '?' {
                        let Some(&n) = s.get(p) else {
                            return (Tok::Partial, p);
                        };
                        if n == '>' {
                            return (tok, p + 1);
                        }
                    }
                }
                return (Tok::Partial, p);
            }
            Bt::Quest => {
                let Some(tok) = check_pi_target(&s[target..p]) else {
                    return (Tok::Invalid, p);
                };
                p += 1;
                let Some(&n) = s.get(p) else {
                    return (Tok::Partial, p);
                };
                if n == '>' {
                    return (tok, p + 1);
                }
                return (Tok::Invalid, p);
            }
            _ if is_namechar(c) => p += 1,
            _ => return (Tok::Invalid, p),
        }
    }
    (Tok::Partial, p)
}

/// `scanPercent` from after `%`.
pub(crate) fn scan_percent(s: &[char], mut p: usize) -> (Tok, usize) {
    let Some(&c) = s.get(p) else {
        return (Tok::Partial, p);
    };
    if matches!(bt(c), Bt::S | Bt::Lf | Bt::Cr | Bt::Percnt) {
        return (Tok::Percent, p);
    }
    if !is_nmstrt(c) {
        return (Tok::Invalid, p);
    }
    p += 1;
    while let Some(&c) = s.get(p) {
        if c == ';' {
            return (Tok::ParamEntityRef, p + 1);
        }
        if !is_namechar(c) {
            return (Tok::Invalid, p);
        }
        p += 1;
    }
    (Tok::Partial, p)
}

/// `scanPoundName` from after `#`; a name at the end of the input is
/// still the token (`-XML_TOK_POUND_NAME`).
fn scan_pound_name(s: &[char], mut p: usize) -> (Tok, usize) {
    let Some(&c) = s.get(p) else {
        return (Tok::Partial, p);
    };
    if !is_nmstrt(c) {
        return (Tok::Invalid, p);
    }
    p += 1;
    while let Some(&c) = s.get(p) {
        match bt(c) {
            Bt::Cr | Bt::Lf | Bt::S | Bt::Rpar | Bt::Gt | Bt::Percnt | Bt::Verbar => {
                return (Tok::PoundName, p);
            }
            _ if is_namechar(c) => p += 1,
            _ => return (Tok::Invalid, p),
        }
    }
    (Tok::PoundName, p)
}

/// `scanLit` from after the opening quote `open`.
fn scan_lit(s: &[char], mut p: usize, open: char) -> (Tok, usize) {
    while let Some(&c) = s.get(p) {
        if is_invalid(c) {
            return (Tok::Invalid, p);
        }
        p += 1;
        if c == open {
            let Some(&n) = s.get(p) else {
                return (Tok::Literal, p);
            };
            return match bt(n) {
                Bt::S | Bt::Cr | Bt::Lf | Bt::Gt | Bt::Percnt | Bt::Lsqb => (Tok::Literal, p),
                _ => (Tok::Invalid, p),
            };
        }
    }
    (Tok::Partial, p)
}

/// `prologTok`: the token at `p` and where it ends (where the scan
/// stopped for `Invalid`). Tokens cut by the end of the input are the
/// tokens themselves, as for the last buffer.
pub(crate) fn prolog_tok(s: &[char], mut p: usize) -> (Tok, usize) {
    let Some(&c) = s.get(p) else {
        return (Tok::None, p);
    };
    let mut tok = match bt(c) {
        Bt::Quot => return scan_lit(s, p + 1, '"'),
        Bt::Apos => return scan_lit(s, p + 1, '\''),
        Bt::Lt => {
            p += 1;
            let Some(&n) = s.get(p) else {
                return (Tok::Partial, p);
            };
            return match bt(n) {
                Bt::Excl => scan_decl(s, p + 1),
                Bt::Quest => scan_pi(s, p + 1),
                Bt::Nmstrt | Bt::Hex => (Tok::InstanceStart, p - 1),
                _ if !n.is_ascii() => (Tok::InstanceStart, p - 1),
                _ => (Tok::Invalid, p),
            };
        }
        Bt::S | Bt::Lf | Bt::Cr => {
            p += 1;
            while s
                .get(p)
                .is_some_and(|c| matches!(bt(*c), Bt::S | Bt::Lf | Bt::Cr))
            {
                p += 1;
            }
            return (Tok::PrologS, p);
        }
        Bt::Percnt => return scan_percent(s, p + 1),
        Bt::Comma => return (Tok::Comma, p + 1),
        Bt::Lsqb => return (Tok::OpenBracket, p + 1),
        Bt::Rsqb => {
            p += 1;
            if s.get(p) == Some(&']') {
                let Some(&n) = s.get(p + 1) else {
                    return (Tok::Partial, p);
                };
                if n == '>' {
                    return (Tok::CondSectClose, p + 2);
                }
            }
            return (Tok::CloseBracket, p);
        }
        Bt::Lpar => return (Tok::OpenParen, p + 1),
        Bt::Rpar => {
            p += 1;
            let Some(&n) = s.get(p) else {
                return (Tok::CloseParen, p);
            };
            return match bt(n) {
                Bt::Ast => (Tok::CloseParenAsterisk, p + 1),
                Bt::Quest => (Tok::CloseParenQuestion, p + 1),
                Bt::Plus => (Tok::CloseParenPlus, p + 1),
                Bt::Cr | Bt::Lf | Bt::S | Bt::Gt | Bt::Comma | Bt::Verbar | Bt::Rpar => {
                    (Tok::CloseParen, p)
                }
                _ => (Tok::Invalid, p),
            };
        }
        Bt::Verbar => return (Tok::Or, p + 1),
        Bt::Gt => return (Tok::DeclClose, p + 1),
        Bt::Num => return scan_pound_name(s, p + 1),
        Bt::Nmstrt | Bt::Hex => Tok::Name,
        Bt::Digit | Bt::Name | Bt::Minus | Bt::Colon => Tok::Nmtoken,
        Bt::NonAscii if is_nmstrt(c) => Tok::Name,
        Bt::NonAscii if is_namechar(c) => Tok::Nmtoken,
        _ => return (Tok::Invalid, p),
    };
    p += 1;
    while let Some(&c) = s.get(p) {
        match bt(c) {
            Bt::Gt
            | Bt::Rpar
            | Bt::Comma
            | Bt::Verbar
            | Bt::Lsqb
            | Bt::Percnt
            | Bt::S
            | Bt::Cr
            | Bt::Lf => return (tok, p),
            Bt::Colon => {
                p += 1;
                match tok {
                    Tok::Name => {
                        let Some(&n) = s.get(p) else {
                            return (Tok::Partial, p);
                        };
                        tok = Tok::PrefixedName;
                        if is_namechar(n) {
                            p += 1;
                        } else if !n.is_ascii() {
                            return (Tok::Invalid, p);
                        } else {
                            tok = Tok::Nmtoken;
                        }
                    }
                    Tok::PrefixedName => tok = Tok::Nmtoken,
                    _ => {}
                }
            }
            Bt::Plus | Bt::Ast | Bt::Quest => {
                if tok == Tok::Nmtoken {
                    return (Tok::Invalid, p);
                }
                let t = match bt(c) {
                    Bt::Plus => Tok::NamePlus,
                    Bt::Ast => Tok::NameAsterisk,
                    _ => Tok::NameQuestion,
                };
                return (t, p + 1);
            }
            _ if is_namechar(c) => p += 1,
            _ => return (Tok::Invalid, p),
        }
    }
    (tok, p)
}

/// The roles `xmlrole.c` gives the tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Error,
    None,
    XmlDecl,
    InstanceStart,
    DoctypeNone,
    DoctypeName,
    DoctypeSystemId,
    DoctypePublicId,
    DoctypeInternalSubset,
    DoctypeClose,
    GeneralEntityName,
    ParamEntityName,
    EntityNone,
    EntityValue,
    EntitySystemId,
    EntityPublicId,
    EntityComplete,
    EntityNotationName,
    NotationNone,
    NotationName,
    NotationSystemId,
    NotationNoSystemId,
    NotationPublicId,
    AttributeName,
    AttributeTypeCdata,
    AttributeTypeTokenized,
    AttributeEnumValue,
    AttributeNotationValue,
    AttlistNone,
    AttlistElementName,
    ImpliedAttributeValue,
    RequiredAttributeValue,
    DefaultAttributeValue,
    FixedAttributeValue,
    ElementNone,
    ElementName,
    ContentAny,
    ContentEmpty,
    ContentPcdata,
    GroupOpen,
    GroupClose,
    GroupChoice,
    GroupSequence,
    ContentElement,
    Pi,
    Comment,
    ParamEntityRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handler {
    Prolog0,
    Prolog1,
    Prolog2,
    Doctype0,
    Doctype1,
    Doctype2,
    Doctype3,
    Doctype4,
    Doctype5,
    InternalSubset,
    Entity0,
    Entity1,
    Entity2,
    Entity3,
    Entity4,
    Entity5,
    Entity6,
    Entity7,
    Entity8,
    Entity9,
    Entity10,
    Notation0,
    Notation1,
    Notation2,
    Notation3,
    Notation4,
    Attlist0,
    Attlist1,
    Attlist2,
    Attlist3,
    Attlist4,
    Attlist5,
    Attlist6,
    Attlist7,
    Attlist8,
    Attlist9,
    Element0,
    Element1,
    Element2,
    Element3,
    Element4,
    Element5,
    Element6,
    Element7,
    DeclClose,
    Error,
}

/// `PROLOG_STATE` of the document entity.
pub(crate) struct PrologState {
    handler: Handler,
    /// The nesting of content model groups.
    pub(crate) level: usize,
    role_none: Role,
}

impl PrologState {
    /// `XmlPrologStateInit`.
    pub(crate) fn new() -> PrologState {
        PrologState {
            handler: Handler::Prolog0,
            level: 0,
            role_none: Role::None,
        }
    }

    /// `common`.
    fn common(&mut self) -> Role {
        self.handler = Handler::Error;
        Role::Error
    }

    fn decl_close(&mut self, role_none: Role) {
        self.handler = Handler::DeclClose;
        self.role_none = role_none;
    }

    /// `XmlTokenRole` of the token `tok`, whose text is `text`.
    pub(crate) fn role(&mut self, tok: Tok, text: &[char]) -> Role {
        use Handler as H;
        let is = |word: &str| text.iter().copied().eq(word.chars());
        let decl = |word: &str| text.len() > 2 && text[2..].iter().copied().eq(word.chars());
        let pound = |word: &str| text.len() > 1 && text[1..].iter().copied().eq(word.chars());
        let s = tok == Tok::PrologS;
        match self.handler {
            H::Prolog0 | H::Prolog1 => match tok {
                Tok::PrologS => {
                    self.handler = H::Prolog1;
                    Role::None
                }
                Tok::XmlDecl if self.handler == H::Prolog0 => {
                    self.handler = H::Prolog1;
                    Role::XmlDecl
                }
                Tok::Pi => {
                    self.handler = H::Prolog1;
                    Role::Pi
                }
                Tok::Comment => {
                    self.handler = H::Prolog1;
                    Role::Comment
                }
                Tok::DeclOpen if decl("DOCTYPE") => {
                    self.handler = H::Doctype0;
                    Role::DoctypeNone
                }
                Tok::InstanceStart => {
                    self.handler = H::Error;
                    Role::InstanceStart
                }
                _ => self.common(),
            },
            H::Prolog2 => match tok {
                Tok::PrologS => Role::None,
                Tok::Pi => Role::Pi,
                Tok::Comment => Role::Comment,
                Tok::InstanceStart => {
                    self.handler = H::Error;
                    Role::InstanceStart
                }
                _ => self.common(),
            },
            H::Doctype0 => match tok {
                _ if s => Role::DoctypeNone,
                Tok::Name | Tok::PrefixedName => {
                    self.handler = H::Doctype1;
                    Role::DoctypeName
                }
                _ => self.common(),
            },
            H::Doctype1 => match tok {
                _ if s => Role::DoctypeNone,
                Tok::OpenBracket => {
                    self.handler = H::InternalSubset;
                    Role::DoctypeInternalSubset
                }
                Tok::DeclClose => {
                    self.handler = H::Prolog2;
                    Role::DoctypeClose
                }
                Tok::Name if is("SYSTEM") => {
                    self.handler = H::Doctype3;
                    Role::DoctypeNone
                }
                Tok::Name if is("PUBLIC") => {
                    self.handler = H::Doctype2;
                    Role::DoctypeNone
                }
                _ => self.common(),
            },
            H::Doctype2 => match tok {
                _ if s => Role::DoctypeNone,
                Tok::Literal => {
                    self.handler = H::Doctype3;
                    Role::DoctypePublicId
                }
                _ => self.common(),
            },
            H::Doctype3 => match tok {
                _ if s => Role::DoctypeNone,
                Tok::Literal => {
                    self.handler = H::Doctype4;
                    Role::DoctypeSystemId
                }
                _ => self.common(),
            },
            H::Doctype4 => match tok {
                _ if s => Role::DoctypeNone,
                Tok::OpenBracket => {
                    self.handler = H::InternalSubset;
                    Role::DoctypeInternalSubset
                }
                Tok::DeclClose => {
                    self.handler = H::Prolog2;
                    Role::DoctypeClose
                }
                _ => self.common(),
            },
            H::Doctype5 => match tok {
                _ if s => Role::DoctypeNone,
                Tok::DeclClose => {
                    self.handler = H::Prolog2;
                    Role::DoctypeClose
                }
                _ => self.common(),
            },
            H::InternalSubset => match tok {
                _ if s => Role::None,
                Tok::DeclOpen if decl("ENTITY") => {
                    self.handler = H::Entity0;
                    Role::EntityNone
                }
                Tok::DeclOpen if decl("ATTLIST") => {
                    self.handler = H::Attlist0;
                    Role::AttlistNone
                }
                Tok::DeclOpen if decl("ELEMENT") => {
                    self.handler = H::Element0;
                    Role::ElementNone
                }
                Tok::DeclOpen if decl("NOTATION") => {
                    self.handler = H::Notation0;
                    Role::NotationNone
                }
                Tok::Pi => Role::Pi,
                Tok::Comment => Role::Comment,
                Tok::ParamEntityRef => Role::ParamEntityRef,
                Tok::CloseBracket => {
                    self.handler = H::Doctype5;
                    Role::DoctypeNone
                }
                Tok::None => Role::None,
                _ => self.common(),
            },
            H::Entity0 => match tok {
                _ if s => Role::EntityNone,
                Tok::Percent => {
                    self.handler = H::Entity1;
                    Role::EntityNone
                }
                Tok::Name => {
                    self.handler = H::Entity2;
                    Role::GeneralEntityName
                }
                _ => self.common(),
            },
            H::Entity1 => match tok {
                _ if s => Role::EntityNone,
                Tok::Name => {
                    self.handler = H::Entity7;
                    Role::ParamEntityName
                }
                _ => self.common(),
            },
            H::Entity2 | H::Entity7 => {
                let general = self.handler == H::Entity2;
                match tok {
                    _ if s => Role::EntityNone,
                    Tok::Name if is("SYSTEM") => {
                        self.handler = if general { H::Entity4 } else { H::Entity9 };
                        Role::EntityNone
                    }
                    Tok::Name if is("PUBLIC") => {
                        self.handler = if general { H::Entity3 } else { H::Entity8 };
                        Role::EntityNone
                    }
                    Tok::Literal => {
                        self.decl_close(Role::EntityNone);
                        Role::EntityValue
                    }
                    _ => self.common(),
                }
            }
            H::Entity3 | H::Entity8 => match tok {
                _ if s => Role::EntityNone,
                Tok::Literal => {
                    self.handler = if self.handler == H::Entity3 {
                        H::Entity4
                    } else {
                        H::Entity9
                    };
                    Role::EntityPublicId
                }
                _ => self.common(),
            },
            H::Entity4 | H::Entity9 => match tok {
                _ if s => Role::EntityNone,
                Tok::Literal => {
                    self.handler = if self.handler == H::Entity4 {
                        H::Entity5
                    } else {
                        H::Entity10
                    };
                    Role::EntitySystemId
                }
                _ => self.common(),
            },
            H::Entity5 => match tok {
                _ if s => Role::EntityNone,
                Tok::DeclClose => {
                    self.handler = H::InternalSubset;
                    Role::EntityComplete
                }
                Tok::Name if is("NDATA") => {
                    self.handler = H::Entity6;
                    Role::EntityNone
                }
                _ => self.common(),
            },
            H::Entity6 => match tok {
                _ if s => Role::EntityNone,
                Tok::Name => {
                    self.decl_close(Role::EntityNone);
                    Role::EntityNotationName
                }
                _ => self.common(),
            },
            H::Entity10 => match tok {
                _ if s => Role::EntityNone,
                Tok::DeclClose => {
                    self.handler = H::InternalSubset;
                    Role::EntityComplete
                }
                _ => self.common(),
            },
            H::Notation0 => match tok {
                _ if s => Role::NotationNone,
                Tok::Name => {
                    self.handler = H::Notation1;
                    Role::NotationName
                }
                _ => self.common(),
            },
            H::Notation1 => match tok {
                _ if s => Role::NotationNone,
                Tok::Name if is("SYSTEM") => {
                    self.handler = H::Notation3;
                    Role::NotationNone
                }
                Tok::Name if is("PUBLIC") => {
                    self.handler = H::Notation2;
                    Role::NotationNone
                }
                _ => self.common(),
            },
            H::Notation2 => match tok {
                _ if s => Role::NotationNone,
                Tok::Literal => {
                    self.handler = H::Notation4;
                    Role::NotationPublicId
                }
                _ => self.common(),
            },
            H::Notation3 => match tok {
                _ if s => Role::NotationNone,
                Tok::Literal => {
                    self.decl_close(Role::NotationNone);
                    Role::NotationSystemId
                }
                _ => self.common(),
            },
            H::Notation4 => match tok {
                _ if s => Role::NotationNone,
                Tok::Literal => {
                    self.decl_close(Role::NotationNone);
                    Role::NotationSystemId
                }
                Tok::DeclClose => {
                    self.handler = H::InternalSubset;
                    Role::NotationNoSystemId
                }
                _ => self.common(),
            },
            H::Attlist0 => match tok {
                _ if s => Role::AttlistNone,
                Tok::Name | Tok::PrefixedName => {
                    self.handler = H::Attlist1;
                    Role::AttlistElementName
                }
                _ => self.common(),
            },
            H::Attlist1 => match tok {
                _ if s => Role::AttlistNone,
                Tok::DeclClose => {
                    self.handler = H::InternalSubset;
                    Role::AttlistNone
                }
                Tok::Name | Tok::PrefixedName => {
                    self.handler = H::Attlist2;
                    Role::AttributeName
                }
                _ => self.common(),
            },
            H::Attlist2 => match tok {
                _ if s => Role::AttlistNone,
                Tok::Name if is("CDATA") => {
                    self.handler = H::Attlist8;
                    Role::AttributeTypeCdata
                }
                Tok::Name
                    if [
                        "ID", "IDREF", "IDREFS", "ENTITY", "ENTITIES", "NMTOKEN", "NMTOKENS",
                    ]
                    .iter()
                    .any(|k| is(k)) =>
                {
                    self.handler = H::Attlist8;
                    Role::AttributeTypeTokenized
                }
                Tok::Name if is("NOTATION") => {
                    self.handler = H::Attlist5;
                    Role::AttlistNone
                }
                Tok::OpenParen => {
                    self.handler = H::Attlist3;
                    Role::AttlistNone
                }
                _ => self.common(),
            },
            H::Attlist3 => match tok {
                _ if s => Role::AttlistNone,
                Tok::Nmtoken | Tok::Name | Tok::PrefixedName => {
                    self.handler = H::Attlist4;
                    Role::AttributeEnumValue
                }
                _ => self.common(),
            },
            H::Attlist4 | H::Attlist7 => match tok {
                _ if s => Role::AttlistNone,
                Tok::CloseParen => {
                    self.handler = H::Attlist8;
                    Role::AttlistNone
                }
                Tok::Or => {
                    self.handler = if self.handler == H::Attlist4 {
                        H::Attlist3
                    } else {
                        H::Attlist6
                    };
                    Role::AttlistNone
                }
                _ => self.common(),
            },
            H::Attlist5 => match tok {
                _ if s => Role::AttlistNone,
                Tok::OpenParen => {
                    self.handler = H::Attlist6;
                    Role::AttlistNone
                }
                _ => self.common(),
            },
            H::Attlist6 => match tok {
                _ if s => Role::AttlistNone,
                Tok::Name => {
                    self.handler = H::Attlist7;
                    Role::AttributeNotationValue
                }
                _ => self.common(),
            },
            H::Attlist8 => match tok {
                _ if s => Role::AttlistNone,
                Tok::PoundName if pound("IMPLIED") => {
                    self.handler = H::Attlist1;
                    Role::ImpliedAttributeValue
                }
                Tok::PoundName if pound("REQUIRED") => {
                    self.handler = H::Attlist1;
                    Role::RequiredAttributeValue
                }
                Tok::PoundName if pound("FIXED") => {
                    self.handler = H::Attlist9;
                    Role::AttlistNone
                }
                Tok::Literal => {
                    self.handler = H::Attlist1;
                    Role::DefaultAttributeValue
                }
                _ => self.common(),
            },
            H::Attlist9 => match tok {
                _ if s => Role::AttlistNone,
                Tok::Literal => {
                    self.handler = H::Attlist1;
                    Role::FixedAttributeValue
                }
                _ => self.common(),
            },
            H::Element0 => match tok {
                _ if s => Role::ElementNone,
                Tok::Name | Tok::PrefixedName => {
                    self.handler = H::Element1;
                    Role::ElementName
                }
                _ => self.common(),
            },
            H::Element1 => match tok {
                _ if s => Role::ElementNone,
                Tok::Name if is("EMPTY") => {
                    self.decl_close(Role::ElementNone);
                    Role::ContentEmpty
                }
                Tok::Name if is("ANY") => {
                    self.decl_close(Role::ElementNone);
                    Role::ContentAny
                }
                Tok::OpenParen => {
                    self.handler = H::Element2;
                    self.level = 1;
                    Role::GroupOpen
                }
                _ => self.common(),
            },
            H::Element2 => match tok {
                _ if s => Role::ElementNone,
                Tok::PoundName if pound("PCDATA") => {
                    self.handler = H::Element3;
                    Role::ContentPcdata
                }
                Tok::OpenParen => {
                    self.level = 2;
                    self.handler = H::Element6;
                    Role::GroupOpen
                }
                Tok::Name
                | Tok::PrefixedName
                | Tok::NameQuestion
                | Tok::NameAsterisk
                | Tok::NamePlus => {
                    self.handler = H::Element7;
                    Role::ContentElement
                }
                _ => self.common(),
            },
            H::Element3 => match tok {
                _ if s => Role::ElementNone,
                Tok::CloseParen | Tok::CloseParenAsterisk => {
                    self.decl_close(Role::ElementNone);
                    Role::GroupClose
                }
                Tok::Or => {
                    self.handler = H::Element4;
                    Role::ElementNone
                }
                _ => self.common(),
            },
            H::Element4 => match tok {
                _ if s => Role::ElementNone,
                Tok::Name | Tok::PrefixedName => {
                    self.handler = H::Element5;
                    Role::ContentElement
                }
                _ => self.common(),
            },
            H::Element5 => match tok {
                _ if s => Role::ElementNone,
                Tok::CloseParenAsterisk => {
                    self.decl_close(Role::ElementNone);
                    Role::GroupClose
                }
                Tok::Or => {
                    self.handler = H::Element4;
                    Role::ElementNone
                }
                _ => self.common(),
            },
            H::Element6 => match tok {
                _ if s => Role::ElementNone,
                Tok::OpenParen => {
                    self.level += 1;
                    Role::GroupOpen
                }
                Tok::Name
                | Tok::PrefixedName
                | Tok::NameQuestion
                | Tok::NameAsterisk
                | Tok::NamePlus => {
                    self.handler = H::Element7;
                    Role::ContentElement
                }
                _ => self.common(),
            },
            H::Element7 => match tok {
                _ if s => Role::ElementNone,
                Tok::CloseParen
                | Tok::CloseParenAsterisk
                | Tok::CloseParenQuestion
                | Tok::CloseParenPlus => {
                    self.level -= 1;
                    if self.level == 0 {
                        self.decl_close(Role::ElementNone);
                    }
                    Role::GroupClose
                }
                Tok::Comma => {
                    self.handler = H::Element6;
                    Role::GroupSequence
                }
                Tok::Or => {
                    self.handler = H::Element6;
                    Role::GroupChoice
                }
                _ => self.common(),
            },
            H::DeclClose => match tok {
                _ if s => self.role_none,
                Tok::DeclClose => {
                    self.handler = H::InternalSubset;
                    self.role_none
                }
                _ => self.common(),
            },
            H::Error => Role::None,
        }
    }
}

/// `scanRef` from after `&`: `Tok::Name` for an entity reference,
/// `Tok::PoundName` for a character reference.
pub(crate) fn scan_ref(s: &[char], mut p: usize) -> (Tok, usize) {
    let Some(&c) = s.get(p) else {
        return (Tok::Partial, p);
    };
    if c == '#' {
        p += 1;
        let hex = s.get(p) == Some(&'x');
        if hex {
            p += 1;
        }
        let digit = |c: char| {
            if hex {
                c.is_ascii_hexdigit()
            } else {
                c.is_ascii_digit()
            }
        };
        let Some(&first) = s.get(p) else {
            return (Tok::Partial, p);
        };
        if !digit(first) {
            return (Tok::Invalid, p);
        }
        p += 1;
        while let Some(&c) = s.get(p) {
            if c == ';' {
                return (Tok::PoundName, p + 1);
            }
            if !digit(c) {
                return (Tok::Invalid, p);
            }
            p += 1;
        }
        return (Tok::Partial, p);
    }
    if !is_nmstrt(c) {
        return (Tok::Invalid, p);
    }
    p += 1;
    while let Some(&c) = s.get(p) {
        if c == ';' {
            return (Tok::Name, p + 1);
        }
        if !is_namechar(c) {
            return (Tok::Invalid, p);
        }
        p += 1;
    }
    (Tok::Partial, p)
}

/// The start of a name after a colon of a qualified name
/// (`CHECK_NMSTRT_CASES` after `BT_COLON`): the position after it, or the
/// token it ends with.
fn after_colon(s: &[char], p: usize) -> Result<usize, (Tok, usize)> {
    match s.get(p) {
        None => Err((Tok::Partial, p)),
        Some(&c) if is_nmstrt(c) => Ok(p + 1),
        Some(_) => Err((Tok::Invalid, p)),
    }
}

/// `scanAtts` from the first character of an attribute name: `Tok::Name`
/// for a start tag, `Tok::NamePlus` for an empty element.
fn scan_atts(s: &[char], mut p: usize) -> (Tok, usize) {
    let mut had_colon = false;
    while let Some(&c) = s.get(p) {
        match bt(c) {
            Bt::Colon => {
                if had_colon {
                    return (Tok::Invalid, p);
                }
                had_colon = true;
                match after_colon(s, p + 1) {
                    Ok(n) => p = n,
                    Err(t) => return t,
                }
            }
            Bt::S | Bt::Cr | Bt::Lf | Bt::Equals => {
                if bt(c) != Bt::Equals {
                    loop {
                        p += 1;
                        let Some(&n) = s.get(p) else {
                            return (Tok::Partial, p);
                        };
                        match bt(n) {
                            Bt::Equals => break,
                            Bt::S | Bt::Lf | Bt::Cr => {}
                            _ => return (Tok::Invalid, p),
                        }
                    }
                }
                had_colon = false;
                let open = loop {
                    p += 1;
                    let Some(&n) = s.get(p) else {
                        return (Tok::Partial, p);
                    };
                    match bt(n) {
                        Bt::Quot | Bt::Apos => break n,
                        Bt::S | Bt::Lf | Bt::Cr => {}
                        _ => return (Tok::Invalid, p),
                    }
                };
                p += 1;
                loop {
                    let Some(&n) = s.get(p) else {
                        return (Tok::Partial, p);
                    };
                    if n == open {
                        break;
                    }
                    match bt(n) {
                        Bt::NonXml | Bt::Lt => return (Tok::Invalid, p),
                        Bt::Amp => match scan_ref(s, p + 1) {
                            (Tok::Name | Tok::PoundName, next) => p = next,
                            other => return other,
                        },
                        _ => p += 1,
                    }
                }
                p += 1;
                let Some(&n) = s.get(p) else {
                    return (Tok::Partial, p);
                };
                match bt(n) {
                    Bt::S | Bt::Cr | Bt::Lf => {}
                    Bt::Sol => return empty_close(s, p + 1),
                    Bt::Gt => return (Tok::Name, p + 1),
                    _ => return (Tok::Invalid, p),
                }
                loop {
                    p += 1;
                    let Some(&n) = s.get(p) else {
                        return (Tok::Partial, p);
                    };
                    match bt(n) {
                        Bt::S | Bt::Cr | Bt::Lf => {}
                        Bt::Gt => return (Tok::Name, p + 1),
                        Bt::Sol => return empty_close(s, p + 1),
                        _ if is_nmstrt(n) => {
                            p += 1;
                            break;
                        }
                        _ => return (Tok::Invalid, p),
                    }
                }
            }
            _ if is_namechar(c) => p += 1,
            _ => return (Tok::Invalid, p),
        }
    }
    (Tok::Partial, p)
}

/// The `>` of `/>` at `p`.
fn empty_close(s: &[char], p: usize) -> (Tok, usize) {
    match s.get(p) {
        None => (Tok::Partial, p),
        Some('>') => (Tok::NamePlus, p + 1),
        Some(_) => (Tok::Invalid, p),
    }
}

/// `scanLt` for a start tag from after `<`: `Tok::Name` for a start tag,
/// `Tok::NamePlus` for an empty element.
pub(crate) fn scan_start_tag(s: &[char], mut p: usize) -> (Tok, usize) {
    match s.get(p) {
        None => return (Tok::Partial, p),
        Some(&c) if is_nmstrt(c) => p += 1,
        Some(_) => return (Tok::Invalid, p),
    }
    let mut had_colon = false;
    while let Some(&c) = s.get(p) {
        match bt(c) {
            Bt::Colon => {
                if had_colon {
                    return (Tok::Invalid, p);
                }
                had_colon = true;
                match after_colon(s, p + 1) {
                    Ok(n) => p = n,
                    Err(t) => return t,
                }
            }
            Bt::S | Bt::Cr | Bt::Lf => {
                p += 1;
                while let Some(&n) = s.get(p) {
                    match bt(n) {
                        Bt::Gt => return (Tok::Name, p + 1),
                        Bt::Sol => return empty_close(s, p + 1),
                        Bt::S | Bt::Cr | Bt::Lf => p += 1,
                        _ if is_nmstrt(n) => return scan_atts(s, p + 1),
                        _ => return (Tok::Invalid, p),
                    }
                }
                return (Tok::Partial, p);
            }
            Bt::Gt => return (Tok::Name, p + 1),
            Bt::Sol => return empty_close(s, p + 1),
            _ if is_namechar(c) => p += 1,
            _ => return (Tok::Invalid, p),
        }
    }
    (Tok::Partial, p)
}

/// `scanEndTag` from after `</`.
pub(crate) fn scan_end_tag(s: &[char], mut p: usize) -> (Tok, usize) {
    match s.get(p) {
        None => return (Tok::Partial, p),
        Some(&c) if is_nmstrt(c) => p += 1,
        Some(_) => return (Tok::Invalid, p),
    }
    while let Some(&c) = s.get(p) {
        match bt(c) {
            Bt::S | Bt::Cr | Bt::Lf => {
                p += 1;
                while let Some(&n) = s.get(p) {
                    match bt(n) {
                        Bt::S | Bt::Cr | Bt::Lf => p += 1,
                        Bt::Gt => return (Tok::Name, p + 1),
                        _ => return (Tok::Invalid, p),
                    }
                }
                return (Tok::Partial, p);
            }
            Bt::Colon => p += 1,
            Bt::Gt => return (Tok::Name, p + 1),
            _ if is_namechar(c) => p += 1,
            _ => return (Tok::Invalid, p),
        }
    }
    (Tok::Partial, p)
}
