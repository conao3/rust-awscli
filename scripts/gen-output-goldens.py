#!/usr/bin/env python3
"""Generate golden outputs for `src/pyfmt.rs` and `src/output/` by running
the reference AWS CLI's Python formatters.

Usage (inside `nix develop`, which sets `$AWSR_REFERENCE_SRC`):

    python3 scripts/gen-output-goldens.py && cargo fmt

Writes `src/pyfmt/goldens.rs` and `src/output/goldens.rs`. Input values are
encoded as JSON with bytes as `{"$bytes": "<base64>"}`; outputs are Rust
string literals.
"""

import argparse
import base64
import copy
import io
import json
import math
import os
import struct
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.environ["AWSR_REFERENCE_SRC"])

import awsr_progname  # noqa: F401, E402

import colorama  # noqa: E402
import jmespath  # noqa: E402

import awscli.table  # noqa: E402
from awscli import errorformat, errorhandler, text  # noqa: E402
from awscli.formatter import YAMLDumper, get_formatter  # noqa: E402
from awscli.utils import json_encoder  # noqa: E402
from botocore.exceptions import ClientError  # noqa: E402
from botocore.paginate import PageIterator  # noqa: E402


# --- encoding helpers -------------------------------------------------------


def enc_value(v):
    if isinstance(v, bytes):
        return {"$bytes": base64.b64encode(v).decode()}
    if isinstance(v, dict):
        return {k: enc_value(x) for k, x in v.items()}
    if isinstance(v, list):
        return [enc_value(x) for x in v]
    return v


def enc(v):
    return json.dumps(enc_value(v), ensure_ascii=False)


def rs(s):
    if s is None:
        return "None"
    out = ['"']
    for ch in s:
        o = ord(ch)
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\r":
            out.append("\\r")
        elif 0x20 <= o < 0x7F or (o >= 0xA0 and ch.isprintable()):
            out.append(ch)
        else:
            out.append("\\u{%x}" % o)
    out.append('"')
    return "".join(out)


def opt(s):
    return "None" if s is None else f"Some({rs(s)})"


# --- pyfmt ------------------------------------------------------------------

FLOATS = [
    0.0, -0.0, 1.0, -1.0, 0.1, 0.5, 1.5, 2.5, 1 / 3, 2 / 3, 100.0, 123.456,
    1e-4, 1e-5, 1.23e-7, 0.00012345, 1e15, 1e16, 1e17, 1.5e17, 1e22, 1e100,
    1.7976931348623157e308, 5e-324, 2.2250738585072014e-308, 123456789.123,
    9007199254740993.0, 1234567890123456.7, 12345678901234567.0, 0.3,
    float("nan"), float("inf"), float("-inf"), 4.35, 1e-07, 99999999999999.99,
    3.141592653589793, -2.5e-10, 6.02214076e23,
]

PY_STR_VALUES = [
    None, True, False, 0, -5, 10**40, -(10**30), 1.0, 1e-05, 1e16, float("nan"),
    "plain", "", "it's", 'say "hi"', "both ' and \"", "tab\tnl\nret\r",
    "\x00\x07\x1b\x7f", "é日本　 ​ ﻿\U0001F600\U000E0001퟿",
    "back\\slash", b"", b"bytes", b"\x00\xff'", b'q"', b"'\"", b"\t\n\r\\",
    [], {}, [1, "x", None, True, 1.5, b"b", [2], {"k": "v"}],
    {"a": 1, "b": [1, 2], "c": {"d": None}, "it's": "x"},
]

JSON_VALUES = [
    None, True, False, 0, 10**30, -7, 1.0, -0.0, 1e-05, 1e16, 1.5e300, 0.1,
    float("nan"), float("inf"), float("-inf"),
    "", "plain", "quote\" back\\ / slash", "\x00\x01\x08\x0c\x1f\x7f\x80",
    "é日本\U0001F600 ﻿", b"", b"\x00\x01binary\xff", [], {},
    [[]], [{}], {"a": []}, {"a": {}},
    {"b": 1, "a": [1, 2.5, None, True, False, "s", b"x"], "c": {"d": {"e": [[], {}]}}},
    [1, [2, [3, [4]]]],
    {"日本": "語", "emoji": "\U0001F600"},
]


def gen_pyfmt():
    lines = ["//! Goldens produced by `scripts/gen-output-goldens.py`. Do not edit.", ""]
    lines.append("#[rustfmt::skip]")
    lines.append("pub(super) static FLOAT_REPR: &[(u64, &str)] = &[")
    for f in FLOATS:
        bits = struct.unpack("<Q", struct.pack("<d", f))[0]
        lines.append(f"    (0x{bits:016x}, {rs(repr(f))}),")
    lines.append("];")
    lines.append("")
    lines.append("/// (input, `str()`, `repr()`)")
    lines.append("#[rustfmt::skip]")
    lines.append("pub(super) static PY_STR: &[(&str, &str, &str)] = &[")
    for v in PY_STR_VALUES:
        lines.append(f"    ({rs(enc(v))}, {rs(str(v))}, {rs(repr(v))}),")
    lines.append("];")
    lines.append("")
    lines.append("/// (input, pretty, default, line)")
    lines.append("#[rustfmt::skip]")
    lines.append("pub(super) static JSON: &[(&str, &str, &str, &str)] = &[")
    for v in JSON_VALUES:
        pretty = json.dumps(v, indent=4, default=json_encoder, ensure_ascii=False)
        line =json.dumps(v, ensure_ascii=False, default=json_encoder)
        default = json.dumps(v, default=json_encoder)
        lines.append(f"    ({rs(enc(v))}, {rs(pretty)}, {rs(default)}, {rs(line)}),")
    lines.append("];")
    return "\n".join(lines) + "\n"


def _flatten(v):
    if isinstance(v, dict):
        for x in v.values():
            yield from _flatten(x)
    elif isinstance(v, list):
        for x in v:
            yield from _flatten(x)
    else:
        yield v


# --- formatters -------------------------------------------------------------


class FakePageIterator(PageIterator):
    def __init__(self, pages, result_keys, non_aggregate_part, resume_token, full_result):
        self._fake_pages = pages
        self._fake_result_keys = [jmespath.compile(k) for k in result_keys]
        self._fake_non_aggregate = non_aggregate_part
        self._fake_resume_token = resume_token
        self._fake_full = full_result

    def __iter__(self):
        return iter(copy.deepcopy(self._fake_pages))

    @property
    def result_keys(self):
        return self._fake_result_keys

    @property
    def non_aggregate_part(self):
        return copy.deepcopy(self._fake_non_aggregate)

    @property
    def resume_token(self):
        return self._fake_resume_token

    def build_full_result(self):
        return copy.deepcopy(self._fake_full)


class Patched:
    """Stand in for stdout being a tty of a given width."""

    def __init__(self, tty, width):
        self.tty = tty
        self.width = width

    def __enter__(self):
        self.saved = (awscli.table.is_a_tty, awscli.table.determine_terminal_width)
        awscli.table.is_a_tty = lambda: self.tty
        awscli.table.determine_terminal_width = lambda default_width=80: self.width
        return self

    def __exit__(self, *a):
        awscli.table.is_a_tty, awscli.table.determine_terminal_width = self.saved
        colorama.deinit()
        colorama.initialise._wipe_internal_state_for_tests()


def run_formatter(fmt, command, response, query=None, color="auto", tty=False, width=80):
    """Returns (stdout, error message or None)."""
    real_stdout = sys.stdout
    buf = io.StringIO()
    sys.stdout = buf
    err = None
    colorized = False
    try:
        with Patched(tty, width):
            args = argparse.Namespace(
                query=jmespath.compile(query) if query is not None else None, color=color
            )
            formatter = get_formatter(fmt, args)
            colorized = fmt == "table" and (color == "on" or (color == "auto" and tty))
            try:
                formatter(command, response)
            except Exception as e:  # noqa: BLE001
                err = str(e)
    finally:
        sys.stdout = real_stdout
    out = buf.getvalue()
    if colorized and tty:
        # colorama registers `reset_all` with atexit, which writes
        # Style.RESET_ALL to a tty stdout when the process exits.
        out += "\x1b[0m"
    return out, err


CORPUS = {
    "nested": {
        "b": 1, "a": [1, 2, {"x": None, "y": True}], "c": {}, "d": [], "e": [[1, 2], [3]],
        "f": 1.5, "g": 1e-05, "h": 1e16, "i": float("nan"), "j": float("inf"),
        "k": float("-inf"), "l": -0.0, "m": 10**30, "n": False, "o": "text",
        "ResponseMetadata": {"RequestId": "abc", "HTTPStatusCode": 200},
    },
    "empty": {},
    "only_metadata": {"ResponseMetadata": {"RequestId": "abc"}},
    "list_of_dicts": {
        "Items": [
            {"Name": "alpha", "Id": 1, "Tags": [{"Key": "k1", "Value": "v1"}], "State": {"Code": 16, "Name": "running"}},
            {"Name": "beta", "Id": 2, "Extra": "only here"},
            {"Name": "gamma", "Id": 3, "Tags": [], "Nested": {"Deep": {"Deeper": [1, 2, 3]}}},
        ],
        "Count": 3,
    },
    "scalars_list": {"Names": ["a", "bb", "ccc"], "Numbers": [1, 2.5, None, True]},
    "mixed_list": {"Mixed": [1, "two", [3, 4], [5, [6]], {"seven": 7}]},
    "list_of_lists": {"Rows": [[1, 2, 3], [4, 5, 6]]},
    "unicode": {
        "Ja": "日本語のテキスト", "Wide": "ＡＢＣ", "Ambiguous": "±×÷αβγ", "Emoji": "\U0001F600",
        "Combining": "é", "Accents": "café", "List": ["日本", "ab", "±"],
    },
    "bytes": {"Blob": b"\x00\x01binary\xff", "Empty": b"", "Long": b"hello world" * 10},
    "strings": {
        "yes": "yes", "no": "no", "on": "on", "null": "null", "tilde": "~", "true": "true",
        "True": "True", "int": "123", "octal": "0o17", "leading_zero": "017", "hex": "0x1F",
        "float": "1.5", "exp": "1e3", "dot": ".5", "inf": ".inf", "nan": ".NaN", "date": "2020-01-01",
        "datetime": "2020-01-01T00:00:00Z", "sexagesimal": "1:20", "colon_space": "a: b",
        "hash": "a #b", "lead_hash": "#a", "mid_hash": "a#b", "quote": "'a", "dquote": '"a',
        "inner_quote": "a'b", "at": "@a", "backtick": "`a", "percent": "%a", "amp": "&a",
        "star": "*a", "bang": "!a", "pipe": "|a", "gt": ">a", "q": "?a", "q_alone": "?",
        "colon": ":a", "trailing_colon": "a:", "lead_space": " a", "trail_space": "a ",
        "newline": "a\nb", "tab": "a\tb", "ctrl": "\x01", "del": "\x7f", "nel": "\u0085",
        "ls": " ", "bom": "﻿", "nbsp": "a b", "dash": "-", "dash_space": "- a",
        "dashes": "---", "dots": "...", "bracket": "[a", "brace": "{a", "comma": ",a",
        "mid_comma": "a,b", "mid_bracket": "a[b]", "plus": "+1", "minus": "-1", "empty": "",
        "merge": "<<", "eq": "=", "backslash": "a\\b", "underscore_int": "1_000",
        "emoji": "smile \U0001F600", "spaces": "a  b   c", "crlf": "a\r\nb",
    },
    "long_strings": {
        "long": "word " * 30,
        "long_nospace": "a" * 100,
        "fold_mixed": "a " * 50 + "b" * 90 + " c",
        "multiline": "line1\nline2\n\nline4\n",
        "trail_nl": "abc\n",
        "lead_nl": "\nabc",
        "quoted_long": "it's a 'quote' " * 8,
        "dq_long": "tab\there " * 12,
        "dq_spaces": "x\t" + "y  " * 40,
        "unicode_long": "あいうえお" * 30,
        "unicode_words": "日本語 テキスト " * 12,
        "break_space": "a\n b",
        "space_break": "a \nb",
    },
    "keys": {"k" * 130: 1, "multi\nkey": 2, "": 3, "1": 4, "true": 5, "a b": 6, "#x": 7, "日本": 8},
    "numbers": {
        "floats": [1.0, 100.0, 1e15, 1e16, 1.5e17, 1e22, 1.23e-7, 0.0001, 0.00001, 123456789.123, -2.5],
        "ints": [0, -1, 2**63, -(2**70)],
    },
    "nested_lists": [[], {}, [[]], [{}], {"a": [[1]]}, [[1, [2, [3]]]]],
    "deep": {"a": {"b": {"c": [{"d": 1, "e": [1, 2]}, {"d": 2}]}}},
    "wide_table": {
        "Instances": [
            {"InstanceId": "i-0123456789abcdef0", "InstanceType": "t3.micro", "State": "running",
             "PrivateIpAddress": "10.0.0.1", "PublicDnsName": "ec2-1-2-3-4.compute-1.amazonaws.com"},
            {"InstanceId": "i-0fedcba9876543210", "InstanceType": "m5.large", "State": "stopped",
             "PrivateIpAddress": "10.0.0.2", "PublicDnsName": ""},
        ]
    },
    "single_wide_row": {
        "Config": {"AlphaSetting": "value-number-one", "BetaSetting": "value-number-two",
                   "GammaSetting": "value-number-three", "DeltaSetting": "value-number-four"}
    },
    "single_key": {"Value": "only"},
    "long_title_section": {"AVeryLongSectionNameThatIsWiderThanItsContentsForSure": [{"a": 1}]},
    "falsy": {"Zero": 0, "False": False, "None": None, "EmptyStr": "", "EmptyList": [], "EmptyMap": {}},
    "scalar_top_str": "just a string",
    "scalar_top_int": 42,
    "scalar_top_none": None,
    "top_list": [{"a": 1, "b": "x"}, {"a": 2, "b": "y"}],
    "top_bytes": b"\x00\x01binary",
    "dict_in_scalar_list": {"L": [1, {"k": "v", "j": "w"}]},
    "ragged_rows": {"L": [[1, 2], [3]]},
    "mixed_dict_list": {"L": [{"a": 1}, "str"]},
}

TEXT_CORPUS = dict(CORPUS)


def gen_output_cases():
    cases = []

    def add(name, fmt, value, query=None, color="auto", tty=False, width=80, command="TestOperation"):
        out, err = run_formatter(fmt, command, copy.deepcopy(value), query, color, tty, width)
        cases.append((name, fmt, command, query, color, tty, width, None, enc(value), out, err))

    for name, value in CORPUS.items():
        for fmt in ("json", "yaml", "yaml-stream", "text", "table", "off"):
            add(f"{name}/{fmt}", fmt, value)
    for name in ("list_of_dicts", "unicode", "wide_table", "single_wide_row", "deep", "scalars_list"):
        for width in (20, 40, 120, 200):
            add(f"{name}/table/w{width}", "table", CORPUS[name], width=width)
        add(f"{name}/table/color-on", "table", CORPUS[name], color="on")
        add(f"{name}/table/color-off-tty", "table", CORPUS[name], color="off", tty=True)
        add(f"{name}/table/tty-auto", "table", CORPUS[name], tty=True, width=100)
    queries = [
        ("list_of_dicts", "Items[].Name"),
        ("list_of_dicts", "Items[].Id"),
        ("list_of_dicts", "Count"),
        ("list_of_dicts", "Missing"),
        ("list_of_dicts", "Items[?Id > `1`].Name"),
        ("list_of_dicts", "length(Items)"),
        ("list_of_dicts", "Items[0].Tags"),
        ("scalars_list", "Names"),
        ("scalars_list", "Names[0]"),
        ("nested", "a"),
        ("nested", "c"),
        ("nested", "d"),
        ("nested", "f"),
        ("nested", "keys(@)"),
    ]
    for name, q in queries:
        for fmt in ("json", "yaml", "yaml-stream", "text", "table"):
            add(f"{name}/{fmt}/query:{q}", fmt, CORPUS[name], query=q)

    # Paginated responses.
    pag = [
        {
            "name": "two_pages",
            "pages": [
                {"Items": [{"Id": 1, "Name": "a"}], "Marker": "m1", "Owner": "me",
                 "ResponseMetadata": {"RequestId": "r1"}},
                {"Items": [{"Id": 2, "Name": "b"}, {"Id": 3, "Name": "c"}], "Owner": "me",
                 "ResponseMetadata": {"RequestId": "r2"}},
            ],
            "result_keys": ["Items"],
            "non_aggregate_part": {"Owner": "me"},
            "resume_token": None,
            "full_result": {"Items": [{"Id": 1, "Name": "a"}, {"Id": 2, "Name": "b"}, {"Id": 3, "Name": "c"}],
                            "Owner": "me"},
        },
        {
            "name": "resume_token",
            "pages": [
                {"Names": ["x", "y"], "ResponseMetadata": {"RequestId": "r1"}},
            ],
            "result_keys": ["Names"],
            "non_aggregate_part": {},
            "resume_token": "next-page-2",
            "full_result": {"Names": ["x", "y"], "NextToken": "next-page-2"},
        },
        {
            "name": "nested_result_key",
            "pages": [
                {"Result": {"Rows": [1, 2]}, "Total": 5},
                {"Result": {"Rows": [3]}, "Total": 5},
            ],
            "result_keys": ["Result.Rows", "Missing"],
            "non_aggregate_part": {"Total": 5},
            "resume_token": None,
            "full_result": {"Result": {"Rows": [1, 2, 3]}, "Total": 5},
        },
        {
            "name": "empty_pages",
            "pages": [{"Items": []}],
            "result_keys": ["Items"],
            "non_aggregate_part": {},
            "resume_token": None,
            "full_result": {"Items": []},
        },
    ]
    for p in pag:
        for fmt in ("json", "yaml", "yaml-stream", "text", "table", "off"):
            for q in (None, "Items[].Name" if p["name"] == "two_pages" else None):
                if q is None and fmt in ("json",) and False:
                    continue
                pi = FakePageIterator(p["pages"], p["result_keys"], p["non_aggregate_part"],
                                      p["resume_token"], p["full_result"])
                out, err = run_formatter(fmt, "ListThings", pi, q)
                spec = {k: p[k] for k in ("pages", "result_keys", "non_aggregate_part", "resume_token", "full_result")}
                name = f"paginated/{p['name']}/{fmt}" + (f"/query:{q}" if q else "")
                if any(c[0] == name for c in cases):
                    continue
                cases.append((name, fmt, "ListThings", q, "auto", False, 80, enc(spec), None, out, err))
    return cases


def gen_error_cases():
    cases = []

    def handle(parsed, op, modeled, fmt, color="auto", tty=False, width=80):
        exc = ClientError(copy.deepcopy(parsed), op)
        exc.modeled_fields = set(modeled) if modeled is not None else None
        stdout = io.StringIO()
        stderr = io.StringIO()
        real_stdout = sys.stdout
        sys.stdout = io.StringIO()
        try:
            with Patched(tty, width):
                handler = errorhandler.ClientErrorHandler(None)
                globals_ = argparse.Namespace(cli_error_format=fmt, color=color)
                handler.handle_exception(exc, stdout, stderr, parsed_globals=globals_)
        finally:
            sys.stdout = real_stdout
        error = parsed.get("Error", {})
        md = parsed.get("ResponseMetadata", {})
        retries = md.get("RetryAttempts") if md.get("MaxAttemptsReached") and "RetryAttempts" in md else None
        cases.append((
            op, error.get("Code", "Unknown"), error.get("Message", "Unknown"), retries,
            enc(parsed), None if modeled is None else json.dumps(modeled), fmt, color, tty, width,
            stderr.getvalue(),
        ))

    md = {"RequestId": "req-1", "HTTPStatusCode": 400, "RetryAttempts": 0}
    md_max = dict(md, MaxAttemptsReached=True, RetryAttempts=2)
    errors = [
        ({"Error": {"Message": "Requested resource not found", "Code": "ResourceNotFoundException"},
          "message": "Requested resource not found", "Extra": "x", "ResponseMetadata": md},
         "DescribeTable", ["Code", "Message", "message"]),
        ({"Error": {"Code": "ThrottlingException", "Message": "Rate exceeded"}, "ResponseMetadata": md_max},
         "ListTables", None),
        ({"Error": {"Code": "ConditionalCheckFailedException", "Message": "The conditional request failed"},
          "Item": {"a": {"S": "b"}}, "ResponseMetadata": md},
         "DeleteItem", ["Code", "Message", "Item"]),
        ({"Error": {"Code": "ValidationError", "Message": "  padded message \n"},
          "Reason": "Bad", "Count": 3, "Ratio": 0.5, "Flag": True, "Nothing": None,
          "Small": [1, "two", None], "SmallMap": {"k": "v", "n": 1},
          "Big": [1, 2, 3, 4, 5], "NestedList": [[1]], "Blob": b"xx", "ResponseMetadata": md},
         "Validate", ["Code", "Message", "Reason", "Count", "Ratio", "Flag", "Nothing", "Small",
                      "SmallMap", "Big", "NestedList", "Blob"]),
        ({"Error": {"Code": "AccessDenied", "Message": "Ünïcödé 日本語 message", "Type": "Sender"},
          "ResponseMetadata": md},
         "GetObject", ["code", "message", "type"]),
        ({"Error": {}, "ResponseMetadata": md}, "Empty", None),
        ({"Error": {"Code": "OnlyCode"}, "ResponseMetadata": md}, "NoMessage", None),
        ({"Error": {"Code": "Wide", "Message": "x" * 100}, "ResponseMetadata": md}, "WideOp", None),
        ({"Error": {"Code": "Short", "Message": "m"}, "ResponseMetadata": md}, "ShortOp", None),
    ]
    for parsed, op, modeled in errors:
        for fmt in ("legacy", "json", "yaml", "text", "table", "enhanced", "bogus", "JSON"):
            handle(parsed, op, modeled, fmt)
    for parsed, op, modeled in errors[:1] + errors[-2:]:
        handle(parsed, op, modeled, "table", color="on")
        handle(parsed, op, modeled, "table", tty=True, width=200)
        handle(parsed, op, modeled, "table", width=40)
    return cases


def gen_general_errors():
    out = []
    for m in ["", "simple", "  padded \n", "multi\nline", "　wide space　", "\x1ctrail"]:
        buf = io.StringIO()
        errorformat.write_error(buf, m)
        out.append((m, buf.getvalue()))
    return out


def gen_output():
    lines = ["//! Goldens produced by `scripts/gen-output-goldens.py`. Do not edit.", ""]
    lines.append(
        "/// (name, format, command, query, color, stdout_is_tty, terminal_width,\n"
        "/// paginated spec, single input, expected output, expected error)"
    )
    lines.append(
        "pub(crate) type FormatCase = (&'static str, &'static str, &'static str, Option<&'static str>, "
        "&'static str, bool, usize, Option<&'static str>, Option<&'static str>, &'static str, "
        "Option<&'static str>);"
    )
    lines.append("")
    cases = gen_output_cases()
    # Cases that evaluate JMESPath (`--query`, or the result keys of a
    # paginated text response) are kept apart.
    groups = [
        ("FORMAT", [c for c in cases if c[3] is None and not (c[7] is not None and c[1] == "text")]),
        ("FORMAT_JMES", [c for c in cases if c[3] is not None or (c[7] is not None and c[1] == "text")]),
    ]
    for static, group in groups:
        lines.append("#[rustfmt::skip]")
        lines.append(f"pub(crate) static {static}: &[FormatCase] = &[")
        for name, fmt, command, query, color, tty, width, spec, single, out, err in group:
            lines.append(
                f"    ({rs(name)}, {rs(fmt)}, {rs(command)}, {opt(query)}, {rs(color)}, "
                f"{'true' if tty else 'false'}, {width}, {opt(spec)}, {opt(single)}, {rs(out)}, {opt(err)}),"
            )
        lines.append("];")
        lines.append("")
    lines.append(
        "/// (operation, code, message, max retries, parsed, modeled fields, format,\n"
        "/// color, stdout_is_tty, terminal_width, expected stderr)"
    )
    lines.append(
        "pub(crate) type ClientErrorCase = (&'static str, &'static str, &'static str, Option<u32>, "
        "&'static str, Option<&'static str>, &'static str, &'static str, bool, usize, &'static str);"
    )
    lines.append("")
    lines.append("#[rustfmt::skip]")
    lines.append("pub(crate) static CLIENT_ERROR: &[ClientErrorCase] = &[")
    for op, code, msg, retries, parsed, modeled, fmt, color, tty, width, expected in gen_error_cases():
        lines.append(
            f"    ({rs(op)}, {rs(code)}, {rs(msg)}, {'None' if retries is None else f'Some({retries})'}, "
            f"{rs(parsed)}, {opt(modeled)}, {rs(fmt)}, {rs(color)}, {'true' if tty else 'false'}, "
            f"{width}, {rs(expected)}),"
        )
    lines.append("];")
    lines.append("")
    lines.append("#[rustfmt::skip]")
    lines.append("pub(crate) static GENERAL_ERROR: &[(&str, &str)] = &[")
    for m, expected in gen_general_errors():
        lines.append(f"    ({rs(m)}, {rs(expected)}),")
    lines.append("];")
    return "\n".join(lines) + "\n"


def main():
    pyfmt = gen_pyfmt()
    output = gen_output()
    with open(os.path.join(ROOT, "src/pyfmt/goldens.rs"), "w") as f:
        f.write(pyfmt)
    with open(os.path.join(ROOT, "src/output/goldens.rs"), "w") as f:
        f.write(output)


if __name__ == "__main__":
    main()
