//! Python-compatible formatting of [`Value`]s: `float.__repr__`, `str()`,
//! and `json.dumps`.

use crate::value::Value;

/// Python `repr(float)` / `str(float)` (they are identical in Python 3):
/// shortest round-trip digits, exponent form `1e-05` / `1e+16` outside
/// `1e-4 <= |x| < 1e16`, `.0` kept for integral values, `nan`, `inf`,
/// `-inf`.
pub fn float_repr(f: f64) -> String {
    todo!("pyfmt::float_repr {f}")
}

/// Python `str(value)` as used by the text and table formatters
/// (`True` / `False` / `None`, `b'...'` for bytes, `repr`-style for
/// containers).
pub fn py_str(v: &Value) -> String {
    todo!("pyfmt::py_str {v:?}")
}

/// `json.dumps(v, indent=4, ensure_ascii=False, default=json_encoder)`
/// without a trailing newline. Bytes are emitted as base64 strings
/// (`awscli.utils.json_encoder`); NaN / Infinity as Python emits them.
pub fn json_dumps_pretty(v: &Value) -> String {
    todo!("pyfmt::json_dumps_pretty {v:?}")
}

/// `json.dumps(v)` with Python's defaults (separators `", "` and `": "`,
/// `ensure_ascii=True`), as used for pagination tokens.
pub fn json_dumps_default(v: &Value) -> String {
    todo!("pyfmt::json_dumps_default {v:?}")
}

/// `json.dumps(v, ensure_ascii=False, default=json_encoder)` on one line
/// (separators `", "` and `": "`), as the YAML dumper uses for scalars.
pub fn json_dumps_line(v: &Value) -> String {
    todo!("pyfmt::json_dumps_line {v:?}")
}
