//! The parts of `xml.dom.minidom` (CPython 3.14) that `history show`
//! uses on HTTP bodies: `parseString` (`expatbuilder.ExpatBuilderNS` over
//! expat with namespace processing and without parameter entity parsing),
//! `toxml`, and `toprettyxml`.

use std::collections::HashMap;

/// The characters expat (pyexpat of the reference) starts a name with,
/// besides `:`.
const NAME_START: &[(u32, u32)] = &[
    (0x41, 0x5A),
    (0x5F, 0x5F),
    (0x61, 0x7A),
    (0xC0, 0xD6),
    (0xD8, 0xF6),
    (0xF8, 0x131),
    (0x134, 0x13E),
    (0x141, 0x148),
    (0x14A, 0x17E),
    (0x180, 0x1C3),
    (0x1CD, 0x1F0),
    (0x1F4, 0x1F5),
    (0x1FA, 0x217),
    (0x250, 0x2A8),
    (0x2BB, 0x2C1),
    (0x386, 0x386),
    (0x388, 0x38A),
    (0x38C, 0x38C),
    (0x38E, 0x3A1),
    (0x3A3, 0x3CE),
    (0x3D0, 0x3D6),
    (0x3DA, 0x3DA),
    (0x3DC, 0x3DC),
    (0x3DE, 0x3DE),
    (0x3E0, 0x3E0),
    (0x3E2, 0x3F3),
    (0x401, 0x40C),
    (0x40E, 0x44F),
    (0x451, 0x45C),
    (0x45E, 0x481),
    (0x490, 0x4C4),
    (0x4C7, 0x4C8),
    (0x4CB, 0x4CC),
    (0x4D0, 0x4EB),
    (0x4EE, 0x4F5),
    (0x4F8, 0x4F9),
    (0x531, 0x556),
    (0x559, 0x559),
    (0x561, 0x586),
    (0x5D0, 0x5EA),
    (0x5F0, 0x5F2),
    (0x621, 0x63A),
    (0x641, 0x64A),
    (0x671, 0x6B7),
    (0x6BA, 0x6BE),
    (0x6C0, 0x6CE),
    (0x6D0, 0x6D3),
    (0x6D5, 0x6D5),
    (0x6E5, 0x6E6),
    (0x905, 0x939),
    (0x93D, 0x93D),
    (0x958, 0x961),
    (0x985, 0x98C),
    (0x98F, 0x990),
    (0x993, 0x9A8),
    (0x9AA, 0x9B0),
    (0x9B2, 0x9B2),
    (0x9B6, 0x9B9),
    (0x9DC, 0x9DD),
    (0x9DF, 0x9E1),
    (0x9F0, 0x9F1),
    (0xA05, 0xA0A),
    (0xA0F, 0xA10),
    (0xA13, 0xA28),
    (0xA2A, 0xA30),
    (0xA32, 0xA33),
    (0xA35, 0xA36),
    (0xA38, 0xA39),
    (0xA59, 0xA5C),
    (0xA5E, 0xA5E),
    (0xA72, 0xA74),
    (0xA85, 0xA8B),
    (0xA8D, 0xA8D),
    (0xA8F, 0xA91),
    (0xA93, 0xAA8),
    (0xAAA, 0xAB0),
    (0xAB2, 0xAB3),
    (0xAB5, 0xAB9),
    (0xABD, 0xABD),
    (0xAE0, 0xAE0),
    (0xB05, 0xB0C),
    (0xB0F, 0xB10),
    (0xB13, 0xB28),
    (0xB2A, 0xB30),
    (0xB32, 0xB33),
    (0xB36, 0xB39),
    (0xB3D, 0xB3D),
    (0xB5C, 0xB5D),
    (0xB5F, 0xB61),
    (0xB85, 0xB8A),
    (0xB8E, 0xB90),
    (0xB92, 0xB95),
    (0xB99, 0xB9A),
    (0xB9C, 0xB9C),
    (0xB9E, 0xB9F),
    (0xBA3, 0xBA4),
    (0xBA8, 0xBAA),
    (0xBAE, 0xBB5),
    (0xBB7, 0xBB9),
    (0xC05, 0xC0C),
    (0xC0E, 0xC10),
    (0xC12, 0xC28),
    (0xC2A, 0xC33),
    (0xC35, 0xC39),
    (0xC60, 0xC61),
    (0xC85, 0xC8C),
    (0xC8E, 0xC90),
    (0xC92, 0xCA8),
    (0xCAA, 0xCB3),
    (0xCB5, 0xCB9),
    (0xCDE, 0xCDE),
    (0xCE0, 0xCE1),
    (0xD05, 0xD0C),
    (0xD0E, 0xD10),
    (0xD12, 0xD28),
    (0xD2A, 0xD39),
    (0xD60, 0xD61),
    (0xE01, 0xE2E),
    (0xE30, 0xE30),
    (0xE32, 0xE33),
    (0xE40, 0xE45),
    (0xE81, 0xE82),
    (0xE84, 0xE84),
    (0xE87, 0xE88),
    (0xE8A, 0xE8A),
    (0xE8D, 0xE8D),
    (0xE94, 0xE97),
    (0xE99, 0xE9F),
    (0xEA1, 0xEA3),
    (0xEA5, 0xEA5),
    (0xEA7, 0xEA7),
    (0xEAA, 0xEAB),
    (0xEAD, 0xEAE),
    (0xEB0, 0xEB0),
    (0xEB2, 0xEB3),
    (0xEBD, 0xEBD),
    (0xEC0, 0xEC4),
    (0xF40, 0xF47),
    (0xF49, 0xF69),
    (0x10A0, 0x10C5),
    (0x10D0, 0x10F6),
    (0x1100, 0x1100),
    (0x1102, 0x1103),
    (0x1105, 0x1107),
    (0x1109, 0x1109),
    (0x110B, 0x110C),
    (0x110E, 0x1112),
    (0x113C, 0x113C),
    (0x113E, 0x113E),
    (0x1140, 0x1140),
    (0x114C, 0x114C),
    (0x114E, 0x114E),
    (0x1150, 0x1150),
    (0x1154, 0x1155),
    (0x1159, 0x1159),
    (0x115F, 0x1161),
    (0x1163, 0x1163),
    (0x1165, 0x1165),
    (0x1167, 0x1167),
    (0x1169, 0x1169),
    (0x116D, 0x116E),
    (0x1172, 0x1173),
    (0x1175, 0x1175),
    (0x119E, 0x119E),
    (0x11A8, 0x11A8),
    (0x11AB, 0x11AB),
    (0x11AE, 0x11AF),
    (0x11B7, 0x11B8),
    (0x11BA, 0x11BA),
    (0x11BC, 0x11C2),
    (0x11EB, 0x11EB),
    (0x11F0, 0x11F0),
    (0x11F9, 0x11F9),
    (0x1E00, 0x1E9B),
    (0x1EA0, 0x1EF9),
    (0x1F00, 0x1F15),
    (0x1F18, 0x1F1D),
    (0x1F20, 0x1F45),
    (0x1F48, 0x1F4D),
    (0x1F50, 0x1F57),
    (0x1F59, 0x1F59),
    (0x1F5B, 0x1F5B),
    (0x1F5D, 0x1F5D),
    (0x1F5F, 0x1F7D),
    (0x1F80, 0x1FB4),
    (0x1FB6, 0x1FBC),
    (0x1FBE, 0x1FBE),
    (0x1FC2, 0x1FC4),
    (0x1FC6, 0x1FCC),
    (0x1FD0, 0x1FD3),
    (0x1FD6, 0x1FDB),
    (0x1FE0, 0x1FEC),
    (0x1FF2, 0x1FF4),
    (0x1FF6, 0x1FFC),
    (0x2126, 0x2126),
    (0x212A, 0x212B),
    (0x212E, 0x212E),
    (0x2180, 0x2182),
    (0x3007, 0x3007),
    (0x3021, 0x3029),
    (0x3041, 0x3094),
    (0x30A1, 0x30FA),
    (0x3105, 0x312C),
    (0x4E00, 0x9FA5),
    (0xAC00, 0xD7A3),
];
/// The characters expat continues a name with, besides `:`.
const NAME_CHAR: &[(u32, u32)] = &[
    (0x2D, 0x2E),
    (0x30, 0x39),
    (0x41, 0x5A),
    (0x5F, 0x5F),
    (0x61, 0x7A),
    (0xB7, 0xB7),
    (0xC0, 0xD6),
    (0xD8, 0xF6),
    (0xF8, 0x131),
    (0x134, 0x13E),
    (0x141, 0x148),
    (0x14A, 0x17E),
    (0x180, 0x1C3),
    (0x1CD, 0x1F0),
    (0x1F4, 0x1F5),
    (0x1FA, 0x217),
    (0x250, 0x2A8),
    (0x2BB, 0x2C1),
    (0x2D0, 0x2D1),
    (0x300, 0x345),
    (0x360, 0x361),
    (0x386, 0x38A),
    (0x38C, 0x38C),
    (0x38E, 0x3A1),
    (0x3A3, 0x3CE),
    (0x3D0, 0x3D6),
    (0x3DA, 0x3DA),
    (0x3DC, 0x3DC),
    (0x3DE, 0x3DE),
    (0x3E0, 0x3E0),
    (0x3E2, 0x3F3),
    (0x401, 0x40C),
    (0x40E, 0x44F),
    (0x451, 0x45C),
    (0x45E, 0x481),
    (0x483, 0x486),
    (0x490, 0x4C4),
    (0x4C7, 0x4C8),
    (0x4CB, 0x4CC),
    (0x4D0, 0x4EB),
    (0x4EE, 0x4F5),
    (0x4F8, 0x4F9),
    (0x531, 0x556),
    (0x559, 0x559),
    (0x561, 0x586),
    (0x591, 0x5A1),
    (0x5A3, 0x5B9),
    (0x5BB, 0x5BD),
    (0x5BF, 0x5BF),
    (0x5C1, 0x5C2),
    (0x5C4, 0x5C4),
    (0x5D0, 0x5EA),
    (0x5F0, 0x5F2),
    (0x621, 0x63A),
    (0x640, 0x652),
    (0x660, 0x669),
    (0x670, 0x6B7),
    (0x6BA, 0x6BE),
    (0x6C0, 0x6CE),
    (0x6D0, 0x6D3),
    (0x6D5, 0x6E8),
    (0x6EA, 0x6ED),
    (0x6F0, 0x6F9),
    (0x901, 0x903),
    (0x905, 0x939),
    (0x93C, 0x94D),
    (0x951, 0x954),
    (0x958, 0x963),
    (0x966, 0x96F),
    (0x981, 0x983),
    (0x985, 0x98C),
    (0x98F, 0x990),
    (0x993, 0x9A8),
    (0x9AA, 0x9B0),
    (0x9B2, 0x9B2),
    (0x9B6, 0x9B9),
    (0x9BC, 0x9BC),
    (0x9BE, 0x9C4),
    (0x9C7, 0x9C8),
    (0x9CB, 0x9CD),
    (0x9D7, 0x9D7),
    (0x9DC, 0x9DD),
    (0x9DF, 0x9E3),
    (0x9E6, 0x9F1),
    (0xA02, 0xA02),
    (0xA05, 0xA0A),
    (0xA0F, 0xA10),
    (0xA13, 0xA28),
    (0xA2A, 0xA30),
    (0xA32, 0xA33),
    (0xA35, 0xA36),
    (0xA38, 0xA39),
    (0xA3C, 0xA3C),
    (0xA3E, 0xA42),
    (0xA47, 0xA48),
    (0xA4B, 0xA4D),
    (0xA59, 0xA5C),
    (0xA5E, 0xA5E),
    (0xA66, 0xA74),
    (0xA81, 0xA83),
    (0xA85, 0xA8B),
    (0xA8D, 0xA8D),
    (0xA8F, 0xA91),
    (0xA93, 0xAA8),
    (0xAAA, 0xAB0),
    (0xAB2, 0xAB3),
    (0xAB5, 0xAB9),
    (0xABC, 0xAC5),
    (0xAC7, 0xAC9),
    (0xACB, 0xACD),
    (0xAE0, 0xAE0),
    (0xAE6, 0xAEF),
    (0xB01, 0xB03),
    (0xB05, 0xB0C),
    (0xB0F, 0xB10),
    (0xB13, 0xB28),
    (0xB2A, 0xB30),
    (0xB32, 0xB33),
    (0xB36, 0xB39),
    (0xB3C, 0xB43),
    (0xB47, 0xB48),
    (0xB4B, 0xB4D),
    (0xB56, 0xB57),
    (0xB5C, 0xB5D),
    (0xB5F, 0xB61),
    (0xB66, 0xB6F),
    (0xB82, 0xB83),
    (0xB85, 0xB8A),
    (0xB8E, 0xB90),
    (0xB92, 0xB95),
    (0xB99, 0xB9A),
    (0xB9C, 0xB9C),
    (0xB9E, 0xB9F),
    (0xBA3, 0xBA4),
    (0xBA8, 0xBAA),
    (0xBAE, 0xBB5),
    (0xBB7, 0xBB9),
    (0xBBE, 0xBC2),
    (0xBC6, 0xBC8),
    (0xBCA, 0xBCD),
    (0xBD7, 0xBD7),
    (0xBE7, 0xBEF),
    (0xC01, 0xC03),
    (0xC05, 0xC0C),
    (0xC0E, 0xC10),
    (0xC12, 0xC28),
    (0xC2A, 0xC33),
    (0xC35, 0xC39),
    (0xC3E, 0xC44),
    (0xC46, 0xC48),
    (0xC4A, 0xC4D),
    (0xC55, 0xC56),
    (0xC60, 0xC61),
    (0xC66, 0xC6F),
    (0xC82, 0xC83),
    (0xC85, 0xC8C),
    (0xC8E, 0xC90),
    (0xC92, 0xCA8),
    (0xCAA, 0xCB3),
    (0xCB5, 0xCB9),
    (0xCBE, 0xCC4),
    (0xCC6, 0xCC8),
    (0xCCA, 0xCCD),
    (0xCD5, 0xCD6),
    (0xCDE, 0xCDE),
    (0xCE0, 0xCE1),
    (0xCE6, 0xCEF),
    (0xD02, 0xD03),
    (0xD05, 0xD0C),
    (0xD0E, 0xD10),
    (0xD12, 0xD28),
    (0xD2A, 0xD39),
    (0xD3E, 0xD43),
    (0xD46, 0xD48),
    (0xD4A, 0xD4D),
    (0xD57, 0xD57),
    (0xD60, 0xD61),
    (0xD66, 0xD6F),
    (0xE01, 0xE2E),
    (0xE30, 0xE3A),
    (0xE40, 0xE4E),
    (0xE50, 0xE59),
    (0xE81, 0xE82),
    (0xE84, 0xE84),
    (0xE87, 0xE88),
    (0xE8A, 0xE8A),
    (0xE8D, 0xE8D),
    (0xE94, 0xE97),
    (0xE99, 0xE9F),
    (0xEA1, 0xEA3),
    (0xEA5, 0xEA5),
    (0xEA7, 0xEA7),
    (0xEAA, 0xEAB),
    (0xEAD, 0xEAE),
    (0xEB0, 0xEB9),
    (0xEBB, 0xEBD),
    (0xEC0, 0xEC4),
    (0xEC6, 0xEC6),
    (0xEC8, 0xECD),
    (0xED0, 0xED9),
    (0xF18, 0xF19),
    (0xF20, 0xF29),
    (0xF35, 0xF35),
    (0xF37, 0xF37),
    (0xF39, 0xF39),
    (0xF3E, 0xF47),
    (0xF49, 0xF69),
    (0xF71, 0xF84),
    (0xF86, 0xF8B),
    (0xF90, 0xF95),
    (0xF97, 0xF97),
    (0xF99, 0xFAD),
    (0xFB1, 0xFB7),
    (0xFB9, 0xFB9),
    (0x10A0, 0x10C5),
    (0x10D0, 0x10F6),
    (0x1100, 0x1100),
    (0x1102, 0x1103),
    (0x1105, 0x1107),
    (0x1109, 0x1109),
    (0x110B, 0x110C),
    (0x110E, 0x1112),
    (0x113C, 0x113C),
    (0x113E, 0x113E),
    (0x1140, 0x1140),
    (0x114C, 0x114C),
    (0x114E, 0x114E),
    (0x1150, 0x1150),
    (0x1154, 0x1155),
    (0x1159, 0x1159),
    (0x115F, 0x1161),
    (0x1163, 0x1163),
    (0x1165, 0x1165),
    (0x1167, 0x1167),
    (0x1169, 0x1169),
    (0x116D, 0x116E),
    (0x1172, 0x1173),
    (0x1175, 0x1175),
    (0x119E, 0x119E),
    (0x11A8, 0x11A8),
    (0x11AB, 0x11AB),
    (0x11AE, 0x11AF),
    (0x11B7, 0x11B8),
    (0x11BA, 0x11BA),
    (0x11BC, 0x11C2),
    (0x11EB, 0x11EB),
    (0x11F0, 0x11F0),
    (0x11F9, 0x11F9),
    (0x1E00, 0x1E9B),
    (0x1EA0, 0x1EF9),
    (0x1F00, 0x1F15),
    (0x1F18, 0x1F1D),
    (0x1F20, 0x1F45),
    (0x1F48, 0x1F4D),
    (0x1F50, 0x1F57),
    (0x1F59, 0x1F59),
    (0x1F5B, 0x1F5B),
    (0x1F5D, 0x1F5D),
    (0x1F5F, 0x1F7D),
    (0x1F80, 0x1FB4),
    (0x1FB6, 0x1FBC),
    (0x1FBE, 0x1FBE),
    (0x1FC2, 0x1FC4),
    (0x1FC6, 0x1FCC),
    (0x1FD0, 0x1FD3),
    (0x1FD6, 0x1FDB),
    (0x1FE0, 0x1FEC),
    (0x1FF2, 0x1FF4),
    (0x1FF6, 0x1FFC),
    (0x20D0, 0x20DC),
    (0x20E1, 0x20E1),
    (0x2126, 0x2126),
    (0x212A, 0x212B),
    (0x212E, 0x212E),
    (0x2180, 0x2182),
    (0x3005, 0x3005),
    (0x3007, 0x3007),
    (0x3021, 0x302F),
    (0x3031, 0x3035),
    (0x3041, 0x3094),
    (0x3099, 0x309A),
    (0x309D, 0x309E),
    (0x30A1, 0x30FA),
    (0x30FC, 0x30FE),
    (0x3105, 0x312C),
    (0x4E00, 0x9FA5),
    (0xAC00, 0xD7A3),
];

const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";

const INVALID_TOKEN: &str = "not well-formed (invalid token)";
const SYNTAX: &str = "syntax error";
const NO_ELEMENTS: &str = "no element found";
const UNCLOSED_TOKEN: &str = "unclosed token";
const TAG_MISMATCH: &str = "mismatched tag";
const DUPLICATE_ATTRIBUTE: &str = "duplicate attribute";
const JUNK_AFTER_DOC_ELEMENT: &str = "junk after document element";
const PARAM_ENTITY_REF: &str = "illegal parameter entity reference";
const UNDEFINED_ENTITY: &str = "undefined entity";
const RECURSIVE_ENTITY_REF: &str = "recursive entity reference";
const ASYNC_ENTITY: &str = "asynchronous entity";
const BAD_CHAR_REF: &str = "reference to invalid character number";
const BINARY_ENTITY_REF: &str = "reference to binary entity";
const ATTRIBUTE_EXTERNAL_ENTITY_REF: &str = "reference to external entity in attribute";
const MISPLACED_XML_PI: &str = "XML or text declaration not at start of entity";
const XML_DECL: &str = "XML declaration not well-formed";
const PUBLICID: &str = "illegal character(s) in public id";
const UNBOUND_PREFIX: &str = "unbound prefix";
const UNDECLARING_PREFIX: &str = "must not undeclare prefix";
const RESERVED_PREFIX_XML: &str =
    "reserved prefix (xml) must not be undeclared or bound to another namespace name";
const RESERVED_PREFIX_XMLNS: &str = "reserved prefix (xmlns) must not be declared or undeclared";
const RESERVED_NAMESPACE_URI: &str =
    "prefix must not be bound to one of the reserved namespace names";

/// An `xml.parsers.expat.ExpatError`: the message and the offset, in
/// characters, expat reports it at.
#[derive(Debug)]
pub(crate) struct ExpatError {
    message: &'static str,
    offset: usize,
}

impl ExpatError {
    /// `str(ExpatError)` for the error in `text`.
    fn display(&self, text: &[char]) -> String {
        let before = &text[..self.offset.min(text.len())];
        let line = before.iter().filter(|c| **c == '\n').count() + 1;
        let column = match before.iter().rposition(|c| *c == '\n') {
            Some(i) => before.len() - i - 1,
            None => before.len(),
        };
        format!("{}: line {line}, column {column}", self.message)
    }
}

fn fail<T>(message: &'static str, offset: usize) -> Result<T, ExpatError> {
    Err(ExpatError { message, offset })
}

enum Node {
    Element {
        name: String,
        attrs: Vec<(String, String)>,
        children: Vec<Node>,
    },
    Text(String),
    CData(String),
    Comment(String),
    Pi {
        target: String,
        data: String,
    },
    DocumentType {
        name: String,
        public_id: Option<String>,
        system_id: Option<String>,
        internal_subset: Option<String>,
    },
}

/// A general entity of the internal subset: its replacement text (`None`
/// for an external one) and whether it is unparsed.
struct Entity {
    text: Option<Vec<char>>,
    notation: bool,
    open: bool,
}

fn in_ranges(ranges: &[(u32, u32)], c: char) -> bool {
    let c = c as u32;
    ranges
        .binary_search_by(|(lo, hi)| {
            if *hi < c {
                std::cmp::Ordering::Less
            } else if *lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn is_name_start(c: char) -> bool {
    c == ':' || in_ranges(NAME_START, c)
}

fn is_name_char(c: char) -> bool {
    c == ':' || in_ranges(NAME_CHAR, c)
}

fn is_s(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n')
}

/// `Char` of XML 1.0.
fn is_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..)
}

fn is_pubid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || " \n-'()+,./:=?;!*#@$_%".contains(c)
}

/// A name with at most one colon that neither starts nor ends it
/// (`QName`).
fn is_qname(name: &str) -> bool {
    match name.split_once(':') {
        None => true,
        Some((prefix, local)) => {
            !prefix.is_empty()
                && !local.is_empty()
                && !local.contains(':')
                && local
                    .chars()
                    .next()
                    .is_some_and(|c| in_ranges(NAME_START, c))
        }
    }
}

fn starts(s: &[char], pos: usize, prefix: &str) -> bool {
    prefix
        .chars()
        .enumerate()
        .all(|(i, c)| s.get(pos + i) == Some(&c))
}

fn skip_s(s: &[char], pos: &mut usize) -> bool {
    let start = *pos;
    while s.get(*pos).is_some_and(|c| is_s(*c)) {
        *pos += 1;
    }
    *pos > start
}

fn name(s: &[char], pos: &mut usize) -> Option<String> {
    if !s.get(*pos).is_some_and(|c| is_name_start(*c)) {
        return None;
    }
    let start = *pos;
    while s.get(*pos).is_some_and(|c| is_name_char(*c)) {
        *pos += 1;
    }
    Some(s[start..*pos].iter().collect())
}

fn nmtoken(s: &[char], pos: &mut usize) -> Option<String> {
    let start = *pos;
    while s.get(*pos).is_some_and(|c| is_name_char(*c)) {
        *pos += 1;
    }
    (*pos > start).then(|| s[start..*pos].iter().collect())
}

/// The end of a name token of the prolog: expat's tokenizer ends one
/// only at these characters (`prologTok`).
/// The end of a `)` token, after its quantifier.
fn close_paren_end(s: &[char], pos: usize) -> Result<(), ExpatError> {
    match s.get(pos) {
        None => Ok(()),
        Some(c) if is_s(*c) || ">,|)".contains(*c) => Ok(()),
        Some(_) => fail(INVALID_TOKEN, pos),
    }
}

fn name_end(s: &[char], pos: usize) -> Result<(), ExpatError> {
    match s.get(pos) {
        None => Ok(()),
        Some(c) if is_s(*c) || ">),|[%+*?".contains(*c) => Ok(()),
        Some(_) => fail(INVALID_TOKEN, pos),
    }
}

/// A name of the prolog.
fn prolog_name(s: &[char], pos: &mut usize) -> Result<Option<String>, ExpatError> {
    let n = name(s, pos);
    if n.is_some() {
        name_end(s, *pos)?;
    }
    Ok(n)
}

/// The name of an entity reference after its `&`: a colon is not a name
/// character there (`XML_NS` tokenizer).
fn ref_name(s: &[char], pos: &mut usize) -> Result<Option<String>, ExpatError> {
    let start = *pos;
    let n = name(s, pos);
    if let Some(n) = &n
        && let Some(i) = n.find(':')
    {
        return fail(INVALID_TOKEN, start + n[..i].chars().count());
    }
    Ok(n)
}

/// `S` that must be there.
fn require_s(s: &[char], pos: &mut usize) -> Result<(), ExpatError> {
    if skip_s(s, pos) {
        Ok(())
    } else {
        fail(SYNTAX, *pos)
    }
}

fn expect(s: &[char], pos: &mut usize, c: char) -> Result<(), ExpatError> {
    if s.get(*pos) == Some(&c) {
        *pos += 1;
        Ok(())
    } else {
        fail(INVALID_TOKEN, *pos)
    }
}

fn keyword(s: &[char], pos: &mut usize, word: &str) -> Result<bool, ExpatError> {
    if starts(s, *pos, word) && !s.get(*pos + word.len()).is_some_and(|c| is_name_char(*c)) {
        name_end(s, *pos + word.len())?;
        *pos += word.len();
        Ok(true)
    } else {
        Ok(false)
    }
}

/// A character reference after `&#`.
fn char_ref(s: &[char], pos: &mut usize, amp: usize) -> Result<char, ExpatError> {
    let hex = s.get(*pos) == Some(&'x');
    if hex {
        *pos += 1;
    }
    let start = *pos;
    while s.get(*pos).is_some_and(|c| {
        if hex {
            c.is_ascii_hexdigit()
        } else {
            c.is_ascii_digit()
        }
    }) {
        *pos += 1;
    }
    if *pos == start || s.get(*pos) != Some(&';') {
        return fail(INVALID_TOKEN, *pos);
    }
    let digits: String = s[start..*pos].iter().collect();
    *pos += 1;
    let value = u32::from_str_radix(&digits, if hex { 16 } else { 10 }).unwrap_or(u32::MAX);
    match char::from_u32(value).filter(|c| is_char(*c)) {
        Some(c) => Ok(c),
        None => fail(BAD_CHAR_REF, amp),
    }
}

fn predefined(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    }
}

fn push_text(out: &mut Vec<Node>, text: &str) {
    if let Some(Node::Text(prev)) = out.last_mut() {
        prev.push_str(text);
    } else if !text.is_empty() {
        out.push(Node::Text(text.to_string()));
    }
}

/// The public and system ids of an `ExternalID`.
type ExternalId = (Option<String>, Option<String>);

struct Parser {
    entities: HashMap<String, Entity>,
    /// The attributes the internal subset declares: whether each is
    /// `CDATA` (the others' values are normalized).
    cdata: HashMap<(String, String), bool>,
    has_param_entity_refs: bool,
    keep_processing: bool,
    standalone: bool,
    /// The namespace bindings in scope, innermost last.
    bindings: Vec<(String, String)>,
}

impl Parser {
    /// Whether an undeclared entity is an error (`! dtd->hasParamEntityRefs
    /// || dtd->standalone`); otherwise it is skipped.
    fn check_entity_decl(&self) -> bool {
        !self.has_param_entity_refs || self.standalone
    }

    fn document(&mut self, s: &[char]) -> Result<Vec<Node>, ExpatError> {
        let mut pos = 0;
        if s.first() == Some(&'\u{feff}') {
            pos = 1;
        }
        if starts(s, pos, "<?xml") && s.get(pos + 5).is_some_and(|c| is_s(*c) || *c == '?') {
            self.xml_decl(s, &mut pos)?;
        }
        let mut document = Vec::new();
        let mut doctype = false;
        loop {
            skip_s(s, &mut pos);
            if pos >= s.len() {
                return fail(NO_ELEMENTS, pos);
            }
            if starts(s, pos, "<!--") {
                document.push(self.comment(s, &mut pos)?);
            } else if starts(s, pos, "<?") {
                document.push(self.pi(s, &mut pos)?);
            } else if starts(s, pos, "<!DOCTYPE") && !doctype {
                doctype = true;
                document.push(self.doctype(s, &mut pos)?);
            } else if starts(s, pos, "<!") {
                return fail(SYNTAX, pos);
            } else if s[pos] == '<' {
                break;
            } else if is_char(s[pos]) {
                return fail(SYNTAX, pos);
            } else {
                return fail(INVALID_TOKEN, pos);
            }
        }
        let root = self.element(s, &mut pos, None)?;
        document.push(root);
        loop {
            skip_s(s, &mut pos);
            if pos >= s.len() {
                break;
            }
            if starts(s, pos, "<!--") {
                document.push(self.comment(s, &mut pos)?);
            } else if starts(s, pos, "<?") {
                document.push(self.pi(s, &mut pos)?);
            } else if is_char(s[pos]) {
                return fail(JUNK_AFTER_DOC_ELEMENT, pos);
            } else {
                return fail(INVALID_TOKEN, pos);
            }
        }
        Ok(document)
    }

    /// `XMLDecl`.
    fn xml_decl(&mut self, s: &[char], pos: &mut usize) -> Result<(), ExpatError> {
        *pos += 5;
        let pseudo = |pos: &mut usize, attr: &str| -> Result<Option<String>, ExpatError> {
            let mut p = *pos;
            if !skip_s(s, &mut p) || !starts(s, p, attr) {
                return Ok(None);
            }
            p += attr.len();
            skip_s(s, &mut p);
            if s.get(p) != Some(&'=') {
                return fail(XML_DECL, p);
            }
            p += 1;
            skip_s(s, &mut p);
            let quote = match s.get(p) {
                Some(q @ ('"' | '\'')) => *q,
                _ => return fail(XML_DECL, p),
            };
            p += 1;
            let start = p;
            while s.get(p).is_some_and(|c| *c != quote) {
                p += 1;
            }
            if p >= s.len() {
                return fail(XML_DECL, start);
            }
            let value: String = s[start..p].iter().collect();
            *pos = p + 1;
            Ok(Some(value))
        };
        let start = *pos;
        match pseudo(pos, "version")? {
            Some(v)
                if v.strip_prefix("1.")
                    .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit())) => {}
            _ => return fail(XML_DECL, start),
        }
        if let Some(encoding) = pseudo(pos, "encoding")?
            && !(encoding
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
                && encoding
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)))
        {
            return fail(XML_DECL, *pos);
        }
        match pseudo(pos, "standalone")?.as_deref() {
            None => {}
            Some("yes") => self.standalone = true,
            Some("no") => {}
            Some(_) => return fail(XML_DECL, *pos),
        }
        skip_s(s, pos);
        if starts(s, *pos, "?>") {
            *pos += 2;
            Ok(())
        } else {
            fail(XML_DECL, *pos)
        }
    }

    fn comment(&mut self, s: &[char], pos: &mut usize) -> Result<Node, ExpatError> {
        let open = *pos;
        *pos += 4;
        let start = *pos;
        loop {
            match s.get(*pos) {
                None => return fail(UNCLOSED_TOKEN, open),
                Some('-') if s.get(*pos + 1) == Some(&'-') => {
                    if s.get(*pos + 2) == Some(&'>') {
                        let data = s[start..*pos].iter().collect();
                        *pos += 3;
                        return Ok(Node::Comment(data));
                    }
                    return fail(INVALID_TOKEN, *pos + 2);
                }
                Some(c) if !is_char(*c) => return fail(INVALID_TOKEN, *pos),
                Some(_) => *pos += 1,
            }
        }
    }

    fn pi(&mut self, s: &[char], pos: &mut usize) -> Result<Node, ExpatError> {
        let open = *pos;
        *pos += 2;
        let Some(target) = name(s, pos) else {
            return fail(INVALID_TOKEN, *pos);
        };
        if target.eq_ignore_ascii_case("xml") {
            return fail(MISPLACED_XML_PI, open);
        }
        if target.contains(':') {
            return fail(INVALID_TOKEN, open + 2);
        }
        if starts(s, *pos, "?>") {
            *pos += 2;
            return Ok(Node::Pi {
                target,
                data: String::new(),
            });
        }
        if !skip_s(s, pos) {
            return fail(INVALID_TOKEN, *pos + usize::from(s.get(*pos) == Some(&'?')));
        }
        let start = *pos;
        loop {
            match s.get(*pos) {
                None => return fail(UNCLOSED_TOKEN, open),
                Some('?') if s.get(*pos + 1) == Some(&'>') => {
                    let data = s[start..*pos].iter().collect();
                    *pos += 2;
                    return Ok(Node::Pi { target, data });
                }
                Some(c) if !is_char(*c) => return fail(INVALID_TOKEN, *pos),
                Some(_) => *pos += 1,
            }
        }
    }

    /// A quoted literal: `SystemLiteral`, or `PubidLiteral` when `pubid`.
    fn literal(&mut self, s: &[char], pos: &mut usize, pubid: bool) -> Result<String, ExpatError> {
        let quote = match s.get(*pos) {
            Some(q @ ('"' | '\'')) => *q,
            _ => return fail(SYNTAX, *pos),
        };
        let open = *pos;
        *pos += 1;
        let start = *pos;
        loop {
            match s.get(*pos) {
                None => return fail(UNCLOSED_TOKEN, open),
                Some(c) if *c == quote => break,
                Some(c) if !is_char(*c) => return fail(INVALID_TOKEN, *pos),
                Some(c) if pubid && !is_pubid_char(*c) => return fail(PUBLICID, open),
                Some(_) => *pos += 1,
            }
        }
        let value: String = s[start..*pos].iter().collect();
        *pos += 1;
        match s.get(*pos) {
            Some(c) if !(is_s(*c) || ">%[".contains(*c)) => fail(INVALID_TOKEN, *pos),
            _ => Ok(value),
        }
    }

    /// `ExternalID` (with only the public id when `public_only`).
    fn external_id(
        &mut self,
        s: &[char],
        pos: &mut usize,
        public_only: bool,
    ) -> Result<Option<ExternalId>, ExpatError> {
        if keyword(s, pos, "SYSTEM")? {
            require_s(s, pos)?;
            let system = self.literal(s, pos, false)?;
            return Ok(Some((None, Some(system))));
        }
        if keyword(s, pos, "PUBLIC")? {
            require_s(s, pos)?;
            let public = self.literal(s, pos, true)?;
            let public = public
                .split([' ', '\n'])
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let mut p = *pos;
            if skip_s(s, &mut p) && matches!(s.get(p), Some('"' | '\'')) {
                *pos = p;
                let system = self.literal(s, pos, false)?;
                return Ok(Some((Some(public), Some(system))));
            }
            if public_only {
                return Ok(Some((Some(public), None)));
            }
            return fail(SYNTAX, p);
        }
        Ok(None)
    }

    fn doctype(&mut self, s: &[char], pos: &mut usize) -> Result<Node, ExpatError> {
        let open = *pos;
        *pos += 9;
        if !skip_s(s, pos) {
            return fail(INVALID_TOKEN, *pos);
        }
        let name_at = *pos;
        let Some(raw) = prolog_name(s, pos)? else {
            return fail(SYNTAX, *pos);
        };
        if !is_qname(&raw) {
            return fail(SYNTAX, name_at);
        }
        let name = raw.rsplit(':').next().unwrap_or_default().to_string();
        let mut p = *pos;
        let (mut public_id, mut system_id) = (None, None);
        if skip_s(s, &mut p)
            && let Some((public, system)) = self.external_id(s, &mut p, false)?
        {
            public_id = public;
            system_id = system;
            self.has_param_entity_refs = true;
        }
        *pos = p;
        skip_s(s, pos);
        let mut internal_subset = None;
        if s.get(*pos) == Some(&'[') {
            *pos += 1;
            let start = *pos;
            self.internal_subset(s, pos)?;
            internal_subset = Some(s[start..*pos - 1].iter().collect());
            skip_s(s, pos);
        }
        match s.get(*pos) {
            Some('>') => *pos += 1,
            None => return fail(UNCLOSED_TOKEN, open),
            Some(_) => return fail(SYNTAX, *pos),
        }
        Ok(Node::DocumentType {
            name,
            public_id,
            system_id,
            internal_subset,
        })
    }

    fn internal_subset(&mut self, s: &[char], pos: &mut usize) -> Result<(), ExpatError> {
        loop {
            skip_s(s, pos);
            let at = *pos;
            match s.get(at) {
                None => return fail(SYNTAX, at),
                Some(']') => {
                    *pos += 1;
                    return Ok(());
                }
                Some('%') => {
                    *pos += 1;
                    if name(s, pos).is_none() {
                        return fail(INVALID_TOKEN, *pos);
                    }
                    expect(s, pos, ';')?;
                    self.has_param_entity_refs = true;
                    self.keep_processing = self.standalone;
                }
                Some('<') if starts(s, at, "<!--") => {
                    self.comment(s, pos)?;
                }
                Some('<') if starts(s, at, "<?") => {
                    self.pi(s, pos)?;
                }
                Some('<') if starts(s, at, "<!") => {
                    *pos += 2;
                    let kw_start = *pos;
                    while s
                        .get(*pos)
                        .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
                    {
                        *pos += 1;
                    }
                    if *pos == kw_start {
                        return fail(INVALID_TOKEN, *pos);
                    }
                    match s.get(*pos) {
                        Some('%') if s.get(*pos + 1).is_some_and(|c| is_s(*c) || *c == '%') => {
                            return fail(INVALID_TOKEN, *pos);
                        }
                        Some(c) if is_s(*c) || *c == '%' => {}
                        _ => return fail(INVALID_TOKEN, *pos),
                    }
                    let kw: String = s[kw_start..*pos].iter().collect();
                    match kw.as_str() {
                        "ELEMENT" => self.element_decl(s, pos)?,
                        "ATTLIST" => self.attlist_decl(s, pos)?,
                        "ENTITY" => self.entity_decl(s, pos)?,
                        "NOTATION" => self.notation_decl(s, pos)?,
                        _ => return fail(SYNTAX, at),
                    }
                }
                Some(c) if is_char(*c) => return fail(SYNTAX, at),
                Some(_) => return fail(INVALID_TOKEN, at),
            }
        }
    }

    fn decl_close(&mut self, s: &[char], pos: &mut usize) -> Result<(), ExpatError> {
        skip_s(s, pos);
        match s.get(*pos) {
            Some('>') => {
                *pos += 1;
                Ok(())
            }
            Some(c) if is_char(*c) => fail(SYNTAX, *pos),
            _ => fail(INVALID_TOKEN, *pos),
        }
    }

    fn element_decl(&mut self, s: &[char], pos: &mut usize) -> Result<(), ExpatError> {
        require_s(s, pos)?;
        if prolog_name(s, pos)?.is_none() {
            return fail(SYNTAX, *pos);
        }
        require_s(s, pos)?;
        if !keyword(s, pos, "EMPTY")? && !keyword(s, pos, "ANY")? {
            if s.get(*pos) != Some(&'(') {
                return fail(SYNTAX, *pos);
            }
            self.content_model(s, pos, true)?;
        }
        self.decl_close(s, pos)
    }

    /// A content model group from its `(`: `Mixed` (when `top`) or
    /// `choice` / `seq`, with its quantifier.
    fn content_model(&mut self, s: &[char], pos: &mut usize, top: bool) -> Result<(), ExpatError> {
        *pos += 1;
        skip_s(s, pos);
        if top && starts(s, *pos, "#PCDATA") {
            *pos += 7;
            skip_s(s, pos);
            if s.get(*pos) == Some(&')') {
                *pos += 1;
                if s.get(*pos) == Some(&'*') {
                    *pos += 1;
                    return Ok(());
                }
                return close_paren_end(s, *pos);
            }
            loop {
                skip_s(s, pos);
                match s.get(*pos) {
                    Some('|') => {
                        *pos += 1;
                        skip_s(s, pos);
                        if prolog_name(s, pos)?.is_none() {
                            return fail(SYNTAX, *pos);
                        }
                    }
                    Some(')') if s.get(*pos + 1) == Some(&'*') => {
                        *pos += 2;
                        return Ok(());
                    }
                    _ => return fail(SYNTAX, *pos),
                }
            }
        }
        let mut separator = None;
        loop {
            match s.get(*pos) {
                Some('(') => self.content_model(s, pos, false)?,
                _ => {
                    if prolog_name(s, pos)?.is_none() {
                        return fail(SYNTAX, *pos);
                    }
                    if s.get(*pos).is_some_and(|c| matches!(c, '?' | '*' | '+')) {
                        *pos += 1;
                    }
                }
            }
            skip_s(s, pos);
            match s.get(*pos) {
                Some(')') => {
                    *pos += 1;
                    if s.get(*pos).is_some_and(|c| matches!(c, '?' | '*' | '+')) {
                        *pos += 1;
                        return Ok(());
                    }
                    return close_paren_end(s, *pos);
                }
                Some(c @ ('|' | ',')) if separator.is_none_or(|p| p == *c) => {
                    separator = Some(*c);
                    *pos += 1;
                    skip_s(s, pos);
                }
                _ => return fail(SYNTAX, *pos),
            }
        }
    }

    fn attlist_decl(&mut self, s: &[char], pos: &mut usize) -> Result<(), ExpatError> {
        require_s(s, pos)?;
        let Some(element) = prolog_name(s, pos)? else {
            return fail(SYNTAX, *pos);
        };
        loop {
            let spaced = skip_s(s, pos);
            match s.get(*pos) {
                Some('>') => {
                    *pos += 1;
                    return Ok(());
                }
                _ if !spaced => return fail(SYNTAX, *pos),
                _ => {}
            }
            let Some(attribute) = prolog_name(s, pos)? else {
                return fail(SYNTAX, *pos);
            };
            require_s(s, pos)?;
            let mut cdata = false;
            let mut tokenized = false;
            for k in [
                "IDREFS", "IDREF", "ID", "ENTITY", "ENTITIES", "NMTOKENS", "NMTOKEN",
            ] {
                if keyword(s, pos, k)? {
                    tokenized = true;
                    break;
                }
            }
            if keyword(s, pos, "CDATA")? {
                cdata = true;
            } else if tokenized {
            } else if keyword(s, pos, "NOTATION")? {
                require_s(s, pos)?;
                if s.get(*pos) != Some(&'(') {
                    return fail(SYNTAX, *pos);
                }
                self.enumeration(s, pos, false)?;
            } else if s.get(*pos) == Some(&'(') {
                self.enumeration(s, pos, true)?;
            } else {
                return fail(SYNTAX, *pos);
            }
            require_s(s, pos)?;
            if !keyword(s, pos, "#REQUIRED")? && !keyword(s, pos, "#IMPLIED")? {
                if keyword(s, pos, "#FIXED")? {
                    require_s(s, pos)?;
                }
                let quote = match s.get(*pos) {
                    Some(q @ ('"' | '\'')) => *q,
                    _ => return fail(SYNTAX, *pos),
                };
                let at = *pos;
                *pos += 1;
                self.att_value(s, pos, Some(quote), at, false)?;
            }
            if self.keep_processing {
                self.cdata
                    .entry((element.clone(), attribute))
                    .or_insert(cdata);
            }
        }
    }

    /// An `Enumeration` (of `Nmtoken`s) or `NotationType` (of `Name`s)
    /// from its `(`.
    fn enumeration(
        &mut self,
        s: &[char],
        pos: &mut usize,
        nmtokens: bool,
    ) -> Result<(), ExpatError> {
        *pos += 1;
        loop {
            skip_s(s, pos);
            let token = if nmtokens {
                nmtoken(s, pos)
            } else {
                name(s, pos)
            };
            if token.is_some() {
                name_end(s, *pos)?;
            }
            if token.is_none() {
                return fail(SYNTAX, *pos);
            }
            skip_s(s, pos);
            match s.get(*pos) {
                Some('|') => *pos += 1,
                Some(')') => {
                    *pos += 1;
                    return close_paren_end(s, *pos);
                }
                _ => return fail(SYNTAX, *pos),
            }
        }
    }

    fn entity_decl(&mut self, s: &[char], pos: &mut usize) -> Result<(), ExpatError> {
        if !skip_s(s, pos) {
            let percent = *pos;
            *pos += 1;
            if name(s, pos).is_none() || s.get(*pos) != Some(&';') {
                return fail(INVALID_TOKEN, *pos);
            }
            return fail(SYNTAX, percent);
        }
        let parameter = s.get(*pos) == Some(&'%');
        if parameter {
            let percent = *pos;
            *pos += 1;
            if !skip_s(s, pos) {
                if name(s, pos).is_none() || s.get(*pos) != Some(&';') {
                    return fail(INVALID_TOKEN, *pos);
                }
                return fail(SYNTAX, percent);
            }
        }
        let at = *pos;
        let Some(entity) = prolog_name(s, pos)? else {
            return fail(SYNTAX, *pos);
        };
        if entity.contains(':') {
            return fail(INVALID_TOKEN, at);
        }
        require_s(s, pos)?;
        let mut value = Entity {
            text: None,
            notation: false,
            open: false,
        };
        if matches!(s.get(*pos), Some('"' | '\'')) {
            value.text = Some(self.entity_value(s, pos)?);
        } else if self.external_id(s, pos, false)?.is_some() {
            let mut p = *pos;
            if skip_s(s, &mut p) && keyword(s, &mut p, "NDATA")? {
                if parameter {
                    return fail(SYNTAX, *pos);
                }
                require_s(s, &mut p)?;
                if prolog_name(s, &mut p)?.is_none() {
                    return fail(SYNTAX, p);
                }
                value.notation = true;
                *pos = p;
            }
        } else {
            return fail(SYNTAX, *pos);
        }
        self.decl_close(s, pos)?;
        if !parameter && self.keep_processing {
            self.entities.entry(entity).or_insert(value);
        }
        Ok(())
    }

    /// `EntityValue`: character references replaced, general entity
    /// references kept.
    fn entity_value(&mut self, s: &[char], pos: &mut usize) -> Result<Vec<char>, ExpatError> {
        let quote = s[*pos];
        let open = *pos;
        *pos += 1;
        let mut out = Vec::new();
        loop {
            match s.get(*pos) {
                None => return fail(UNCLOSED_TOKEN, open),
                Some(c) if *c == quote => {
                    *pos += 1;
                    return Ok(out);
                }
                Some('%') => return fail(PARAM_ENTITY_REF, *pos),
                Some('&') => {
                    let amp = *pos;
                    *pos += 1;
                    if s.get(*pos) == Some(&'#') {
                        *pos += 1;
                        out.push(char_ref(s, pos, amp)?);
                    } else {
                        if ref_name(s, pos)?.is_none() {
                            return fail(INVALID_TOKEN, *pos);
                        }
                        expect(s, pos, ';')?;
                        out.extend_from_slice(&s[amp..*pos]);
                    }
                }
                Some(c) if !is_char(*c) => return fail(INVALID_TOKEN, *pos),
                Some(c) => {
                    out.push(*c);
                    *pos += 1;
                }
            }
        }
    }

    fn notation_decl(&mut self, s: &[char], pos: &mut usize) -> Result<(), ExpatError> {
        require_s(s, pos)?;
        if prolog_name(s, pos)?.is_none() {
            return fail(SYNTAX, *pos);
        }
        require_s(s, pos)?;
        if self.external_id(s, pos, true)?.is_none() {
            return fail(SYNTAX, *pos);
        }
        self.decl_close(s, pos)
    }

    /// The value of an attribute from after its opening quote (or the
    /// whole of `s` for an entity's replacement text, `quote` `None`),
    /// with references replaced and white space normalized (`
    /// appendAttributeValue`). Errors in entities are reported at
    /// `at`.
    fn att_value(
        &mut self,
        s: &[char],
        pos: &mut usize,
        quote: Option<char>,
        at: usize,
        content: bool,
    ) -> Result<String, ExpatError> {
        let mut out = String::new();
        loop {
            let c = match s.get(*pos) {
                None if quote.is_none() => return Ok(out),
                None => return fail(UNCLOSED_TOKEN, at),
                Some(c) => *c,
            };
            if Some(c) == quote {
                *pos += 1;
                return Ok(out);
            }
            match c {
                '<' => return fail(INVALID_TOKEN, if quote.is_some() { *pos } else { at }),
                '&' => {
                    let amp = *pos;
                    *pos += 1;
                    if s.get(*pos) == Some(&'#') {
                        *pos += 1;
                        out.push(char_ref(s, pos, amp)?);
                        continue;
                    }
                    let Some(entity) = ref_name(s, pos)? else {
                        return fail(INVALID_TOKEN, *pos);
                    };
                    expect(s, pos, ';')?;
                    if let Some(c) = predefined(&entity) {
                        out.push(c);
                        continue;
                    }
                    let report = if quote.is_some() { amp } else { at };
                    let check = if content {
                        self.check_entity_decl()
                    } else {
                        !self.has_param_entity_refs
                    };
                    let Some(e) = self.entities.get_mut(&entity) else {
                        if check {
                            return fail(UNDEFINED_ENTITY, report);
                        }
                        continue;
                    };
                    if e.open {
                        return fail(RECURSIVE_ENTITY_REF, report);
                    }
                    if e.notation {
                        return fail(BINARY_ENTITY_REF, report);
                    }
                    let Some(text) = e.text.clone() else {
                        return fail(ATTRIBUTE_EXTERNAL_ENTITY_REF, report);
                    };
                    e.open = true;
                    let r = self.att_value(&text, &mut 0, None, at, content);
                    if let Some(e) = self.entities.get_mut(&entity) {
                        e.open = false;
                    }
                    out.push_str(&r.map_err(|e| ExpatError {
                        message: e.message,
                        offset: if quote.is_some() { amp } else { at },
                    })?);
                }
                '\t' | '\n' | '\r' => {
                    out.push(' ');
                    *pos += 1;
                }
                c if !is_char(c) => {
                    return fail(INVALID_TOKEN, if quote.is_some() { *pos } else { at });
                }
                c => {
                    out.push(c);
                    *pos += 1;
                }
            }
        }
    }

    /// The namespace URI `prefix` is bound to.
    fn lookup(&self, prefix: &str) -> Option<&str> {
        if prefix == "xml" {
            return Some(XML_NAMESPACE);
        }
        self.bindings
            .iter()
            .rev()
            .find(|(p, _)| p == prefix)
            .map(|(_, u)| u.as_str())
    }

    /// An element from its `<`, with its namespace declarations first
    /// among its attributes (`ExpatBuilderNS`). `entity` is the position
    /// of the entity reference whose replacement text `s` is.
    fn element(
        &mut self,
        s: &[char],
        pos: &mut usize,
        entity: Option<usize>,
    ) -> Result<Node, ExpatError> {
        let open = *pos;
        let at = entity.unwrap_or(open);
        *pos += 1;
        let Some(tag) = name(s, pos) else {
            return fail(INVALID_TOKEN, entity.unwrap_or(*pos));
        };
        let mut raw: Vec<(String, String, usize)> = Vec::new();
        let empty = loop {
            let spaced = skip_s(s, pos);
            match s.get(*pos) {
                None => return fail(UNCLOSED_TOKEN, at),
                Some('>') => {
                    *pos += 1;
                    break false;
                }
                Some('/') => {
                    *pos += 1;
                    if s.get(*pos) != Some(&'>') {
                        return fail(INVALID_TOKEN, entity.unwrap_or(*pos));
                    }
                    *pos += 1;
                    break true;
                }
                _ if !spaced => return fail(INVALID_TOKEN, entity.unwrap_or(*pos)),
                _ => {}
            }
            let name_at = *pos;
            let Some(attribute) = name(s, pos) else {
                return fail(INVALID_TOKEN, entity.unwrap_or(*pos));
            };
            skip_s(s, pos);
            if s.get(*pos) != Some(&'=') {
                return fail(INVALID_TOKEN, entity.unwrap_or(*pos));
            }
            *pos += 1;
            skip_s(s, pos);
            let quote = match s.get(*pos) {
                Some(q @ ('"' | '\'')) => *q,
                _ => return fail(INVALID_TOKEN, entity.unwrap_or(*pos)),
            };
            *pos += 1;
            let mut value =
                self.att_value(s, pos, Some(quote), at, true)
                    .map_err(|e| ExpatError {
                        message: e.message,
                        offset: entity.unwrap_or(e.offset),
                    })?;
            if raw.iter().any(|(k, _, _)| *k == attribute) {
                return fail(DUPLICATE_ATTRIBUTE, entity.unwrap_or(name_at));
            }
            if self.cdata.get(&(tag.clone(), attribute.clone())) == Some(&false) {
                value = value
                    .split(' ')
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            raw.push((attribute, value, name_at));
        };
        let scope = self.bindings.len();
        let mut declarations = Vec::new();
        let mut attributes = Vec::new();
        for (attribute, value, name_at) in raw {
            let prefix = if attribute == "xmlns" {
                Some(String::new())
            } else {
                attribute.strip_prefix("xmlns:").map(str::to_string)
            };
            match prefix {
                Some(prefix) => {
                    if attribute != "xmlns"
                        && !(prefix
                            .chars()
                            .next()
                            .is_some_and(|c| in_ranges(NAME_START, c))
                            && !prefix.contains(':'))
                    {
                        return fail(INVALID_TOKEN, entity.unwrap_or(name_at));
                    }
                    if prefix == "xmlns" {
                        return fail(RESERVED_PREFIX_XMLNS, at);
                    }
                    if prefix == "xml" && value != XML_NAMESPACE {
                        return fail(RESERVED_PREFIX_XML, at);
                    }
                    if (value == XML_NAMESPACE && prefix != "xml") || value == XMLNS_NAMESPACE {
                        return fail(RESERVED_NAMESPACE_URI, at);
                    }
                    if value.is_empty() && !prefix.is_empty() {
                        return fail(UNDECLARING_PREFIX, at);
                    }
                    if value.contains(' ') {
                        return fail(SYNTAX, at);
                    }
                    self.bindings.push((prefix, value.clone()));
                    declarations.push((attribute, value));
                }
                None => attributes.push((attribute, value, name_at)),
            }
        }
        if !is_qname(&tag) {
            return fail(INVALID_TOKEN, at);
        }
        if let Some((prefix, _)) = tag.split_once(':')
            && (prefix == "xmlns" || self.lookup(prefix).is_none())
        {
            return fail(UNBOUND_PREFIX, at);
        }
        let mut expanded: Vec<(String, String)> = Vec::new();
        for (attribute, _, name_at) in &attributes {
            if !is_qname(attribute) {
                return fail(INVALID_TOKEN, entity.unwrap_or(*name_at));
            }
            let key = match attribute.split_once(':') {
                Some((prefix, local)) => match self.lookup(prefix) {
                    Some(uri) if prefix != "xmlns" => (uri.to_string(), local.to_string()),
                    _ => return fail(UNBOUND_PREFIX, at),
                },
                None => (String::new(), attribute.clone()),
            };
            if expanded.contains(&key) {
                return fail(DUPLICATE_ATTRIBUTE, at);
            }
            expanded.push(key);
        }
        let mut attrs = declarations;
        attrs.extend(attributes.into_iter().map(|(k, v, _)| (k, v)));
        let mut children = Vec::new();
        if !empty {
            self.content(s, pos, &mut children, entity, Some(&tag))?;
        }
        self.bindings.truncate(scope);
        Ok(Node::Element {
            name: tag,
            attrs,
            children,
        })
    }

    /// The content of the element `element` up to its end tag, or the
    /// whole replacement text of the entity referenced at `entity`.
    fn content(
        &mut self,
        s: &[char],
        pos: &mut usize,
        out: &mut Vec<Node>,
        entity: Option<usize>,
        element: Option<&str>,
    ) -> Result<(), ExpatError> {
        let report = |offset: usize| entity.unwrap_or(offset);
        loop {
            let Some(&c) = s.get(*pos) else {
                return match (element, entity) {
                    (None, _) => Ok(()),
                    (Some(_), Some(e)) => fail(ASYNC_ENTITY, e),
                    (Some(_), None) => fail(NO_ELEMENTS, *pos),
                };
            };
            match c {
                '<' => match s.get(*pos + 1) {
                    Some('/') => {
                        let Some(open) = element else {
                            return fail(ASYNC_ENTITY, report(*pos));
                        };
                        let name_at = *pos + 2;
                        *pos += 2;
                        let Some(end) = name(s, pos) else {
                            return fail(INVALID_TOKEN, report(*pos));
                        };
                        if end != open {
                            return fail(TAG_MISMATCH, report(name_at));
                        }
                        skip_s(s, pos);
                        if s.get(*pos) != Some(&'>') {
                            return fail(INVALID_TOKEN, report(*pos));
                        }
                        *pos += 1;
                        return Ok(());
                    }
                    Some('!') if starts(s, *pos, "<!--") => {
                        let node = self.comment(s, pos).map_err(|e| ExpatError {
                            message: e.message,
                            offset: report(e.offset),
                        })?;
                        out.push(node);
                    }
                    Some('!') if starts(s, *pos, "<![CDATA[") => {
                        let open = *pos;
                        *pos += 9;
                        let start = *pos;
                        loop {
                            match s.get(*pos) {
                                None => return fail(UNCLOSED_TOKEN, report(open)),
                                Some(']') if starts(s, *pos, "]]>") => break,
                                Some(c) if !is_char(*c) => {
                                    return fail(INVALID_TOKEN, report(*pos));
                                }
                                Some(_) => *pos += 1,
                            }
                        }
                        let data: String = s[start..*pos].iter().collect();
                        *pos += 3;
                        if !data.is_empty() {
                            out.push(Node::CData(data));
                        }
                    }
                    Some('?') => {
                        let node = self.pi(s, pos).map_err(|e| ExpatError {
                            message: e.message,
                            offset: report(e.offset),
                        })?;
                        out.push(node);
                    }
                    Some(n) if is_name_start(*n) => {
                        let child = self.element(s, pos, entity)?;
                        out.push(child);
                    }
                    _ => return fail(INVALID_TOKEN, report(*pos + 1)),
                },
                '&' => {
                    let amp = *pos;
                    *pos += 1;
                    if s.get(*pos) == Some(&'#') {
                        *pos += 1;
                        let c = char_ref(s, pos, amp).map_err(|e| ExpatError {
                            message: e.message,
                            offset: report(e.offset),
                        })?;
                        push_text(out, &c.to_string());
                        continue;
                    }
                    let Some(name) = ref_name(s, pos).map_err(|e| ExpatError {
                        message: e.message,
                        offset: report(e.offset),
                    })?
                    else {
                        return fail(INVALID_TOKEN, report(*pos));
                    };
                    if s.get(*pos) != Some(&';') {
                        return fail(INVALID_TOKEN, report(*pos));
                    }
                    *pos += 1;
                    if let Some(c) = predefined(&name) {
                        push_text(out, &c.to_string());
                        continue;
                    }
                    let at = report(amp);
                    let check = self.check_entity_decl();
                    let Some(e) = self.entities.get_mut(&name) else {
                        if check {
                            return fail(UNDEFINED_ENTITY, at);
                        }
                        continue;
                    };
                    if e.open {
                        return fail(RECURSIVE_ENTITY_REF, at);
                    }
                    if e.notation {
                        return fail(BINARY_ENTITY_REF, at);
                    }
                    let Some(text) = e.text.clone() else {
                        continue;
                    };
                    e.open = true;
                    let r = self.content(&text, &mut 0, out, Some(at), None);
                    if let Some(e) = self.entities.get_mut(&name) {
                        e.open = false;
                    }
                    r?;
                }
                _ => {
                    let start = *pos;
                    while let Some(&c) = s.get(*pos) {
                        if c == '<' || c == '&' {
                            break;
                        }
                        if !is_char(c) {
                            return fail(INVALID_TOKEN, report(*pos));
                        }
                        if c == ']' && starts(s, *pos, "]]>") {
                            return fail(INVALID_TOKEN, report(*pos + 2));
                        }
                        *pos += 1;
                    }
                    let text: String = s[start..*pos].iter().collect();
                    push_text(out, &text);
                }
            }
        }
    }
}

/// `minidom.parseString(text)`: the document's nodes, or the
/// `ExpatError` expat raises.
fn parse(text: &[char]) -> Result<Vec<Node>, ExpatError> {
    let mut parser = Parser {
        entities: HashMap::new(),
        cdata: HashMap::new(),
        has_param_entity_refs: false,
        keep_processing: true,
        standalone: false,
        bindings: Vec::new(),
    };
    parser.document(text)
}

/// The text expat parses: `\r\n` and `\r` become `\n`.
fn normalized(text: &str) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// `_write_data`.
fn write_data(out: &mut String, text: &str, attr: bool) {
    let mut text = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if attr {
        text = text
            .replace('"', "&quot;")
            .replace('\r', "&#13;")
            .replace('\n', "&#10;")
            .replace('\t', "&#9;");
    }
    out.push_str(&text);
}

/// `Node.writexml(writer, indent, addindent, newl)`.
fn write_node(out: &mut String, node: &Node, indent: &str, addindent: &str, newl: &str) {
    match node {
        Node::Element {
            name,
            attrs,
            children,
        } => {
            out.push_str(indent);
            out.push('<');
            out.push_str(name);
            for (k, v) in attrs {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                write_data(out, v, true);
                out.push('"');
            }
            if children.is_empty() {
                out.push_str("/>");
                out.push_str(newl);
                return;
            }
            out.push('>');
            if let [only @ (Node::Text(_) | Node::CData(_))] = children.as_slice() {
                write_node(out, only, "", "", "");
            } else {
                out.push_str(newl);
                let inner = format!("{indent}{addindent}");
                for child in children {
                    write_node(out, child, &inner, addindent, newl);
                }
                out.push_str(indent);
            }
            out.push_str("</");
            out.push_str(name);
            out.push('>');
            out.push_str(newl);
        }
        Node::Text(data) => write_data(out, &format!("{indent}{data}{newl}"), false),
        Node::CData(data) => {
            out.push_str("<![CDATA[");
            out.push_str(data);
            out.push_str("]]>");
        }
        Node::Comment(data) => {
            out.push_str(indent);
            out.push_str("<!--");
            out.push_str(data);
            out.push_str("-->");
            out.push_str(newl);
        }
        Node::Pi { target, data } => {
            out.push_str(indent);
            out.push_str("<?");
            out.push_str(target);
            out.push(' ');
            out.push_str(data);
            out.push_str("?>");
            out.push_str(newl);
        }
        Node::DocumentType {
            name,
            public_id,
            system_id,
            internal_subset,
        } => {
            out.push_str("<!DOCTYPE ");
            out.push_str(name);
            match (
                public_id.as_deref().filter(|p| !p.is_empty()),
                system_id.as_deref().filter(|s| !s.is_empty()),
            ) {
                (Some(public), system) => out.push_str(&format!(
                    "{newl}  PUBLIC '{public}'{newl}  '{}'",
                    system.unwrap_or_default()
                )),
                (None, Some(system)) => out.push_str(&format!("{newl}  SYSTEM '{system}'")),
                (None, None) => {}
            }
            if let Some(subset) = internal_subset {
                out.push_str(" [");
                out.push_str(subset);
                out.push(']');
            }
            out.push('>');
            out.push_str(newl);
        }
    }
}

/// `Document.toprettyxml(indent, newl)`.
fn toprettyxml(document: &[Node], indent: &str, newl: &str) -> String {
    let mut out = String::from("<?xml version=\"1.0\" ?>");
    out.push_str(newl);
    for node in document {
        write_node(&mut out, node, "", indent, newl);
    }
    out
}

/// `str.isspace` of one character.
fn is_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// `str.splitlines`.
fn splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            lines.push(&s[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' && chars.peek().map(|(_, n)| *n) == Some('\n') {
                chars.next();
                end += 1;
            }
            start = end;
        }
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// `SectionValuePrettyFormatter._get_pretty_xml` of a body `_is_xml`
/// accepts (`None` for one it rejects): the whitespace at the ends of each
/// line of `toxml()` stripped, parsed again, and written with
/// `toprettyxml(indent=' ' * 4, newl='\n')`; `Err` is the `ExpatError`
/// of the second parse.
pub(crate) fn pretty_xml(body: &str) -> Option<Result<String, String>> {
    let document = parse(&normalized(body)).ok()?;
    let xml = toprettyxml(&document, "", "");
    let stripped: String = splitlines(&xml)
        .into_iter()
        .map(|l| l.trim_matches(is_space))
        .collect();
    let text = normalized(&stripped);
    Some(
        parse(&text)
            .map(|document| toprettyxml(&document, "    ", "\n"))
            .map_err(|e| e.display(&text)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pretty(body: &str) -> Option<Result<&'static str, String>> {
        pretty_xml(body).map(|r| r.map(|s| &*s.leak()))
    }

    #[test]
    fn pretty_bodies() {
        assert_eq!(
            pretty("<a xmlns=\"u\" x=\"1\">\n  <b>t &amp; u</b>\n  <c/>\n</a>"),
            Some(Ok(
                "<?xml version=\"1.0\" ?>\n<a xmlns=\"u\" x=\"1\">\n    <b>t &amp; u</b>\n    <c/>\n</a>\n"
            ))
        );
        assert_eq!(
            pretty("<!DOCTYPE a PUBLIC \"-//X//Y\" \"x.dtd\" [<!ENTITY e \"<b/>\">]><a>&e;</a>"),
            Some(Ok(
                "<?xml version=\"1.0\" ?>\n<!DOCTYPE a\n  PUBLIC '-//X//Y'\n  'x.dtd' [<!ENTITY e \"<b/>\">]>\n<a>\n    <b/>\n</a>\n"
            ))
        );
        assert_eq!(
            pretty("<!DOCTYPE a SYSTEM \"a'b\"><a/>"),
            Some(Err(
                "not well-formed (invalid token): line 1, column 45".to_string()
            ))
        );
        assert_eq!(pretty("{\"a\": 1}"), None);
        assert_eq!(pretty("<a><b></a>"), None);
        assert_eq!(pretty("<p:a/>"), None);
        assert_eq!(
            pretty("<!DOCTYPE a SYSTEM \"x\"><a>&e;</a>"),
            Some(Ok(
                "<?xml version=\"1.0\" ?>\n<!DOCTYPE a\n  SYSTEM 'x'>\n<a/>\n"
            ))
        );
    }
}
