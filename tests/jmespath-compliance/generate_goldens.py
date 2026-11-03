"""Generate goldens.json from jmespath.py 1.0.1 (the version the reference
AWS CLI bundles).

Usage: python3 generate_goldens.py <jmespath.py checkout at tag 1.0.1>

Every case of compliance/ and legacy/ plus the extra cases below is run
through jmespath.compile(...).search(...). A result is recorded as
json.dumps(result) (Python's default separators and ensure_ascii), which
keeps dict order and the int / float distinction; an error as the
exception class name and str(exception).
"""

import json
import os
import sys

sys.path.insert(0, sys.argv[1])

import jmespath  # noqa: E402

assert jmespath.__version__ == "1.0.1", jmespath.__version__

HERE = os.path.dirname(os.path.abspath(__file__))

GIVEN = {
    "b": 2,
    "a": 1,
    "c": [3, 1.5, 2],
    "s": ["b", "a", "c"],
    "f": [1.5, 2.5],
    "i": [1, 2, 3],
    "e": [],
    "n": None,
    "t": True,
    "z": 0,
    "o": {"y": 1, "x": 2},
    "big": 12345678901234567890123,
    "u": "caf\u00e9 \u2603 \U0001f600",
    "objs": [
        {"name": "bob", "age": 30, "mixed": 1},
        {"name": "alice", "age": 25, "mixed": "x"},
        {"name": "carol", "age": 30, "mixed": 2},
        {"name": "dave", "age": 25.0},
    ],
    "nested": [[{"x": 1}, {"x": 2}, {"x": 3}]],
    "pairs": [["k", 1], ["j", 2]],
}

EXTRA = [
    "@",
    "*",
    "o",
    "o.*",
    "keys(o)",
    "values(o)",
    "{z: a, a: b, m: c}",
    "{a: a, a: b}",
    "[b, a]",
    "n.{a: b}",
    "n.[a]",
    "merge(o, {a: `1`}, `{\"y\": 3}`)",
    "merge(o, `[]`)",
    "merge(o, pairs)",
    "merge(o, `1`)",
    "merge(o, 'ab')",
    "merge(o, `[[1, 2]]`)",
    "merge(o, `[[\"a\"]]`)",
    "sum(i)",
    "sum(f)",
    "sum(c)",
    "sum(e)",
    "sum([big, big])",
    "sum([`1e16`, `1`, `1`])",
    "sum([`0.1`, `0.2`, `0.3`])",
    "sum([`0.5`, big, `1`])",
    "avg(i)",
    "avg(c)",
    "avg(e)",
    "avg([big])",
    "max(c)",
    "min(c)",
    "max(s)",
    "min(s)",
    "max(e)",
    "max([`1`, `1.0`])",
    "min([`1.0`, `1`])",
    "sort(c)",
    "sort(s)",
    "sort([`2`, `1.0`, `1`, `0.5`])",
    "abs(`-1`)",
    "abs(`-1.5`)",
    "abs(big)",
    "ceil(`1.5`)",
    "floor(`-1.5`)",
    "ceil(`2`)",
    "floor(`1e400`)",
    "ceil(big)",
    "length(u)",
    "reverse(u)",
    "reverse(c)",
    "to_string(u)",
    "to_string(o)",
    "to_string(c)",
    "to_string([u, `1.0`, `null`, `true`])",
    "to_string(`1.0`)",
    "to_string(big)",
    "to_string(`1e400`)",
    "to_number('1.50')",
    "to_number('1e3')",
    "to_number(' 12 ')",
    "to_number('1_000')",
    "to_number('1__000')",
    "to_number('_1')",
    "to_number('1_000.5')",
    "to_number('nan')",
    "to_number('-Infinity')",
    "to_number('inf')",
    "to_number('0x10')",
    "to_number('-0')",
    "to_number('+5')",
    "to_number('.5')",
    "to_number('5.')",
    "to_number('1e')",
    "to_number('')",
    "to_number(`true`)",
    "to_number(big)",
    "to_number('123456789012345678901234567890')",
    "type(big)",
    "type(`1.0`)",
    "`1` == `1.0`",
    "`0` == `false`",
    "`1` == `true`",
    "`[0]` == `[false]`",
    "`{\"a\": 1}` == `{\"a\": 1.0}`",
    "`2` == `2.0`",
    "`1` < `1.5`",
    "big > `1e22`",
    "a < 'x'",
    "'x' >= a",
    "'a' < 'b'",
    "t < `1`",
    "n < `1`",
    "!z",
    "!`0.0`",
    "!e",
    "!n",
    "!'a'",
    "!`1`",
    "z || 'x'",
    "e || 'x'",
    "`0.0` && 'x'",
    "contains(c, `1.5`)",
    "contains(i, `true`)",
    "contains(u, 'caf')",
    "contains(u, `1`)",
    "i[-1]",
    "i[5]",
    "i[::-1]",
    "i[::0]",
    "i[1:]",
    "i[10:]",
    "i[-10:2]",
    "i[:-1:-1]",
    "i[99999999999999999999:]",
    "i[99999999999999999999]",
    "nested[0][1:2]",
    "nested[0][1:2].x",
    "nested[0][1:2][0]",
    "[0][1:2]",
    "s[?@ > 'a']",
    "c[?@ > `1`]",
    "objs[*].name",
    "objs[].[name, age]",
    "objs | [0]",
    "objs[?age > `27`].name",
    "objs[?contains(name, 'a')].name",
    "map(&age, objs)",
    "map(&missing, objs)",
    "sort_by(objs, &age)[*].name",
    "sort_by(objs, &name)[*].name",
    "sort_by(objs, &missing)",
    "sort_by(objs, &mixed)",
    "sort_by(e, &x)",
    "max_by(objs, &age).name",
    "min_by(objs, &age).name",
    "max_by(objs, &mixed)",
    "min_by(objs, &name).name",
    "max_by(e, &x)",
    "max_by(objs, &missing)",
    "foo.",
    "foo[",
    "[1",
    "foo bar",
    "a.1",
    "`{bad`",
    "\"\\x\"",
    "\"\\u00e9\"",
    "\"\\ud83d\\ude00\"",
    "\"\\u12\"",
    "\"a\tb\"",
    "'unclosed",
    "`unclosed",
    "=",
    "a =",
    "-",
    "~",
    "foo(",
    "\"f\"(a)",
    "a.(b)",
    "{a: b",
    "{1: a}",
    "a[1:2:3:4]",
    "a[?b",
    "&",
    "!",
    "@ ==",
    "a == =",
    "a.`1`",
    "a.'x'",
    "[*",
    "f(a b)",
    "{a: b c: d}",
    "a[1 2]",
    "foo()",
    "abs()",
    "abs(`1`, `2`)",
    "not_null()",
    "abs('x')",
    "abs(&a)",
    "avg(s)",
    "join(',', i)",
    "join(',', s)",
    "max(`[1, \"a\"]`)",
    "max(`[\"a\", 1]`)",
    "length(`1`)",
    "length(o)",
    "sort(`[true]`)",
    "type(`{}`)",
    "starts_with(u, 'caf')",
    "ends_with(u, 'x')",
    "not_null(n, z, a)",
    "to_array(a)",
    "to_array(i)",
    "`foo`",
    "`  bar`",
    "`\"x\"`",
    "`1.0`",
    "`-0`",
    "`-0.0`",
    "`1E2`",
    "`[1, 2.0]`",
    "`{\"b\": 1, \"a\": 2}`",
    "`NaN`",
    "`-Infinity`",
    "'raw\\'s'",
    "'raw\\\\n'",
    "\"\"",
    "a.\"b\"(c)",
    "",
    " ",
    "a | b | c",
    "a || b && c",
    "!a == b",
    "a[?b == `1`] | [0]",
]


def run(given, expression):
    try:
        result = jmespath.compile(expression).search(given)
    except Exception as e:
        return {"error_class": type(e).__name__, "error": str(e)}
    try:
        return {"result": json.dumps(result)}
    except TypeError:
        return None


def main():
    cases = []
    for sub in ("compliance", "legacy"):
        directory = os.path.join(HERE, sub)
        for filename in sorted(os.listdir(directory)):
            if filename == "benchmarks.json":
                continue
            with open(os.path.join(directory, filename)) as f:
                suites = json.load(f)
            for suite in suites:
                for case in suite["cases"]:
                    outcome = run(suite["given"], case["expression"])
                    if outcome is not None:
                        cases.append(
                            dict(
                                source="%s/%s" % (sub, filename),
                                given=suite["given"],
                                expression=case["expression"],
                                **outcome,
                            )
                        )
    for expression in EXTRA:
        outcome = run(GIVEN, expression)
        if outcome is not None:
            cases.append(
                dict(source="extra", given=GIVEN, expression=expression, **outcome)
            )
    with open(os.path.join(HERE, "goldens.json"), "w") as f:
        json.dump(cases, f, indent=1)
        f.write("\n")


main()
